//! `AgentApi` — the surface `rc-agent` talks to (§13).

use crate::app::{Admission, App};
use crate::auth;
use crate::images;
use rc_core::cas;
use rc_core::pb::agent_api_server::{AgentApi, AgentApiServer};
use rc_core::pb::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// How many egress rows one project may accumulate. Generous for a real
/// repository — a build reaching more than this many distinct hosts is not a
/// build — and small enough that the approval queue stays a thing a human can
/// read.
const MAX_EGRESS_PER_PROJECT: i64 = 64;

pub struct AgentService {
    app: Arc<App>,
}

impl AgentService {
    pub fn new(app: Arc<App>) -> AgentApiServer<Self> {
        AgentApiServer::new(AgentService { app })
    }

    fn authenticate<T>(&self, req: &Request<T>) -> Result<(), Status> {
        if self.app.cfg.allow_anonymous_agents {
            return Ok(());
        }
        let token = auth::bearer_from_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing agent bearer token"))?;
        match self.app.store.agent_token_valid(&auth::hash_token(&token)) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Status::unauthenticated("unknown agent token")),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    /// Register the hosts a repository asked to reach, and report the ones an
    /// administrator has not approved yet (§7.1).
    ///
    /// Never fatal: a build that cannot reach a host fails on its own terms,
    /// with a network error the agent can act on, and refusing the whole task
    /// would strand every project whose request is still in the queue. The
    /// patterns are re-validated here because the agent that sent them is not
    /// something the control plane trusts.
    fn record_egress_requests(&self, req: &SubmitTaskReq) -> (Vec<String>, Vec<String>) {
        if req.egress.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let hosts = match rc_core::egress::normalize_all(&req.egress) {
            Ok(hosts) => hosts,
            Err(problems) => {
                tracing::warn!(project = %req.project_id, ?problems, "ignoring malformed egress request");
                return (Vec::new(), Vec::new());
            }
        };
        // The approval queue is the one human gate in this design, so it has to
        // stay readable. An agent token is fleet-wide and `project_id` comes off
        // the wire, which means anyone holding a token can file requests under
        // any project; a cap will not stop them forging one, but it does stop a
        // loop from burying every genuine request under a million rows.
        // Counted against rows actually created, not against hosts asked for:
        // the config line stays in the repository, so a project re-requests
        // everything it already has on every single submission, and charging
        // that would put a settled project permanently over its own cap.
        // Existing rows are never disturbed — a decision already made must not
        // be walked back by a flood.
        let mut room =
            MAX_EGRESS_PER_PROJECT - self.app.store.egress_count(&req.project_id).unwrap_or(0);
        let mut refused = Vec::new();
        for host in &hosts {
            if room <= 0 {
                refused.push(host.clone());
                continue;
            }
            match self.app.store.request_egress(&req.project_id, host, &req.agent_session) {
                Ok(created) => {
                    if created {
                        room -= 1;
                    }
                }
                Err(e) => tracing::warn!(%host, error = %e, "could not record the egress request"),
            }
        }
        if !refused.is_empty() {
            tracing::warn!(
                project = %req.project_id, ?refused,
                "egress requests refused: this project is at its approval-queue cap"
            );
        }
        let pending = self.app.store.pending_egress(&req.project_id).unwrap_or_default();
        (pending, refused)
    }
}

fn internal(e: impl std::fmt::Display) -> Status {
    Status::internal(e.to_string())
}

#[tonic::async_trait]
impl AgentApi for AgentService {
    /// Reconciliation is also lease renewal: answering "I have it" bumps
    /// `last_used` so GC cannot delete it out from under the next task (§4.7).
    async fn check_blobs(
        &self,
        req: Request<CheckBlobsReq>,
    ) -> Result<Response<CheckBlobsResp>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        let mut missing = Vec::new();
        let mut present = Vec::new();
        for h in &req.hashes {
            if !cas::is_valid_hash(h) {
                return Err(Status::invalid_argument(format!("malformed hash {h}")));
            }
            if self.app.cas.exists(h) {
                let size = self.app.cas.size_of(h).unwrap_or(0) as i64;
                present.push((h.clone(), size));
            } else {
                missing.push(h.clone());
            }
        }
        self.app.store.touch_blobs(&present).map_err(internal)?;
        self.app
            .metrics
            .incr("blobs_reconciled_total", req.hashes.len() as f64);
        self.app.metrics.incr("blobs_missing_total", missing.len() as f64);
        Ok(Response::new(CheckBlobsResp { missing }))
    }

    async fn upload_blobs(
        &self,
        req: Request<Streaming<BlobChunk>>,
    ) -> Result<Response<UploadBlobsResp>, Status> {
        self.authenticate(&req)?;
        let mut stream = req.into_inner();
        let mut buffers: HashMap<String, Vec<u8>> = HashMap::new();
        let mut accepted = 0u32;
        let mut rejected = Vec::new();
        let mut stored = Vec::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if !cas::is_valid_hash(&chunk.hash) {
                return Err(Status::invalid_argument("malformed blob hash"));
            }
            let buf = buffers.entry(chunk.hash.clone()).or_default();
            buf.extend_from_slice(&chunk.data);
            if !chunk.last {
                continue;
            }
            let data = buffers.remove(&chunk.hash).unwrap_or_default();
            // Verify rather than trust: a wrong hash here would poison every
            // future lookup of that key.
            match self.app.cas.put_verified(&chunk.hash, &data) {
                Ok(()) => {
                    accepted += 1;
                    stored.push((chunk.hash.clone(), data.len() as i64));
                    self.app
                        .metrics
                        .incr("blob_bytes_uploaded_total", data.len() as f64);
                }
                Err(e) => {
                    tracing::warn!(hash = %chunk.hash, error = %e, "rejected blob");
                    rejected.push(chunk.hash.clone());
                }
            }
        }
        self.app.store.touch_blobs(&stored).map_err(internal)?;
        self.app.metrics.incr("blobs_uploaded_total", accepted as f64);
        Ok(Response::new(UploadBlobsResp { accepted, rejected }))
    }

    /// The agent uploaded a git bundle covering commits the fleet could not
    /// fetch (§4.1 step 3).
    async fn register_bundle(&self, req: Request<BundleUpload>) -> Result<Response<Empty>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        if !self.app.cas.exists(&req.blob_hash) {
            return Err(Status::failed_precondition("bundle blob has not been uploaded"));
        }
        self.app
            .store
            .add_bundle(&req.project_id, &req.base_commit, &req.blob_hash)
            .map_err(internal)?;
        self.app
            .store
            .note_known_commit(&req.project_id, &req.base_commit)
            .map_err(internal)?;
        // Pin the bundle so GC never strands a baseline.
        self.app
            .store
            .touch_blobs(&[(req.blob_hash.clone(), 0)])
            .map_err(internal)?;
        Ok(Response::new(Empty {}))
    }

    /// Tell the agent whether it needs to build a bundle at all, and from
    /// which base points.
    async fn get_baseline(&self, req: Request<BaselineReq>) -> Result<Response<BaselineResp>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        let have = self
            .app
            .store
            .has_commit(&req.project_id, &req.base_commit)
            .map_err(internal)?;
        let known = self
            .app
            .store
            .known_commits(&req.project_id, 20)
            .map_err(internal)?;
        Ok(Response::new(BaselineResp {
            have,
            known_commits: known,
            // Workers never hold upstream credentials (§4.1), so we do not
            // promise that a fetch will succeed.
            upstream_public: false,
        }))
    }

    async fn submit_task(&self, req: Request<SubmitTaskReq>) -> Result<Response<TaskHandle>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        let (egress_pending, egress_refused) = self.record_egress_requests(&req);
        match self.app.submit(&req) {
            Ok(Admission::Queued { task_id }) => Ok(Response::new(TaskHandle {
                task_id,
                status: "queued".into(),
                egress_pending,
                egress_refused,
                ..Default::default()
            })),
            Ok(Admission::Subscribed { task_id }) => Ok(Response::new(TaskHandle {
                task_id,
                status: "queued".into(),
                egress_pending,
                egress_refused,
                subscribed: true,
                message: "identical work already in flight; attached to it".into(),
                ..Default::default()
            })),
            // A cache hit is exactly when the agent most needs telling: nothing
            // ran, so nothing will fail with a network error to hint that an
            // approval is still sitting in a queue.
            Ok(Admission::CacheHit { task_id, result }) => Ok(Response::new(TaskHandle {
                task_id,
                status: "done".into(),
                cache_hit: true,
                result: Some(result),
                egress_pending,
                egress_refused,
                ..Default::default()
            })),
            Ok(Admission::NeedsBlobs { missing }) => Ok(Response::new(TaskHandle {
                status: "needs_blobs".into(),
                missing_blobs: missing,
                message: "upload the listed blobs and resubmit".into(),
                egress_pending,
                egress_refused,
                ..Default::default()
            })),
            Err(e) => Err(Status::failed_precondition(e.to_string())),
        }
    }

    /// Long-poll: return as soon as the task reaches a terminal state, or when
    /// `wait_secs` elapses. With `return_on_progress`, also returns when
    /// progress_version advances past `seen_progress_version` (§5.4).
    async fn get_task(&self, req: Request<TaskQuery>) -> Result<Response<TaskStatus>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        // Raised to 120s for cold monorepo long-poll (intent-and-query-surface §7.2).
        let deadline = std::time::Duration::from_secs(req.wait_secs.min(120) as u64);
        let mut rx = self.app.events.subscribe();

        let baseline = if req.baseline.is_empty() {
            "auto".to_string()
        } else {
            req.baseline.clone()
        };
        let current = self
            .app
            .task_status_with_baseline(&req.task_id, &baseline)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("unknown task {}", req.task_id)))?;
        if req.wait_secs == 0 || rc_core::TaskState::parse_or_default(&current.status).is_terminal() {
            return Ok(Response::new(current));
        }
        if req.return_on_progress && current.progress_version > req.seen_progress_version {
            return Ok(Response::new(current));
        }

        let task_id = req.task_id.clone();
        let return_on_progress = req.return_on_progress;
        let seen = req.seen_progress_version;
        let baseline_c = baseline.clone();
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.task_id() == Some(task_id.as_str()) => {
                        if let Ok(Some(st)) =
                            self.app.task_status_with_baseline(&task_id, &baseline_c)
                        {
                            if rc_core::TaskState::parse_or_default(&st.status).is_terminal() {
                                return Some(st);
                            }
                            if return_on_progress && st.progress_version > seen {
                                return Some(st);
                            }
                        }
                    }
                    Ok(_) => {}
                    // Lagged: fall back to a direct read rather than giving up.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Ok(Some(st)) =
                            self.app.task_status_with_baseline(&task_id, &baseline_c)
                        {
                            if rc_core::TaskState::parse_or_default(&st.status).is_terminal() {
                                return Some(st);
                            }
                            if return_on_progress && st.progress_version > seen {
                                return Some(st);
                            }
                        }
                    }
                    Err(_) => return None,
                }
            }
        };

        match tokio::time::timeout(deadline, wait).await {
            Ok(Some(st)) => Ok(Response::new(st)),
            // Timed out or the bus closed: hand back whatever state we have.
            _ => {
                let st = self
                    .app
                    .task_status_with_baseline(&req.task_id, &baseline)
                    .map_err(internal)?
                    .ok_or_else(|| Status::not_found("task vanished"))?;
                Ok(Response::new(st))
            }
        }
    }

    async fn get_log(&self, req: Request<LogQuery>) -> Result<Response<LogChunk>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        self.app.get_log(&req).map(Response::new).map_err(internal)
    }

    /// The bootstrap call for a new project: it answers "what does the fleet
    /// already know about building this?" including a digest-pinned image the
    /// agent can use straight away.
    async fn get_profile(&self, req: Request<GetProfileReq>) -> Result<Response<ProfileResp>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        let stored = self
            .app
            .store
            .get_profile(&req.project_id, &req.path)
            .map_err(internal)?;

        let adapter = stored
            .as_ref()
            .map(|p| p.adapter.clone())
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "rust".into());

        // Prefer the image the profile names, provided it is still trusted.
        let mut resolved = String::new();
        let mut image_status = String::new();
        if let Some(p) = &stored {
            if !p.image.is_empty() {
                let digest = p.image.split_once('@').map(|(_, d)| d).unwrap_or_default();
                if self.app.store.is_digest_trusted(digest).unwrap_or(false) {
                    resolved = p.image.clone();
                    image_status = "healthy".into();
                }
            }
        }
        if resolved.is_empty() {
            if let Ok(Some(fallback)) = images::default_image_for(&self.app, &adapter) {
                resolved = fallback;
                image_status = "healthy".into();
            }
        }

        // Put an approved `pre_commands` back into the profile the fleet hands
        // out. Stored apart from the profile precisely so that this is the only
        // place it can re-enter, and only after someone read it (§3.2).
        let mut config_toml = stored.as_ref().map(|p| p.config_toml.clone()).unwrap_or_default();
        let approved_pre = self
            .app
            .store
            .approved_pre_commands(&req.project_id, &req.path)
            .unwrap_or_default();
        if !approved_pre.is_empty() {
            if let Ok(parsed) = rc_core::profile::parse_toml(&config_toml) {
                let mut p = parsed.profile;
                p.pre_commands = Some(approved_pre);
                config_toml = rc_core::profile::to_toml(&p);
            }
        }
        let pre_pending = self
            .app
            .store
            .pending_pre_commands(&req.project_id, &req.path)
            .unwrap_or(false);
        let pending_pre_commands: Vec<String> = self
            .app
            .store
            .list_pre_commands(Some("pending_approval"))
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.project_id == req.project_id && r.path == req.path)
            .flat_map(|r| r.commands)
            .take(10)
            .collect();

        let message = if resolved.is_empty() {
            "控制面还没有已审批的可用镜像：用 prepare_env 提交 Dockerfile，管理员审批后即可使用（§8.3/§8.4）".to_string()
        } else if stored.is_none() {
            "该项目还没有 Build Profile，返回的是 fleet 默认镜像；首次成功后会自动沉淀".to_string()
        } else if pre_pending {
            // Said out loud because the symptom is invisible: the build simply
            // runs without a codegen step it needed, and fails somewhere else.
            "该项目学到的 pre_commands 还在等管理员审批，本次不会执行它们（§3.2）".to_string()
        } else {
            String::new()
        };

        Ok(Response::new(ProfileResp {
            found: stored.is_some(),
            config_toml,
            health: stored.as_ref().map(|p| ProfileHealth {
                last_success_at: p.last_success_at,
                success_count: p.success_count as u32,
                total_count: p.total_count as u32,
                created_by: p.created_by.clone(),
            }),
            image_status,
            message,
            resolved_image: resolved,
            adapter,
            pending_pre_commands,
        }))
    }

    /// Fleet learning (§1.1 principle 4): what one agent works out, every
    /// other agent inherits.
    async fn upsert_profile(&self, req: Request<UpsertProfileReq>) -> Result<Response<ProfileResp>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        let parsed = rc_core::profile::parse_toml(&req.config_toml)
            .map_err(|e| Status::invalid_argument(format!("invalid profile toml: {e}")))?;

        // §3.2: pointing a profile at an unapproved image would let an agent
        // sidestep image review for everyone else.
        if let Some(image) = &parsed.profile.image {
            let digest = image.split_once('@').map(|(_, d)| d).unwrap_or_default();
            let trusted = self.app.store.is_digest_trusted(digest).unwrap_or(false);
            if !trusted && self.app.policy().require_image_approval {
                return Err(Status::failed_precondition(format!(
                    "profile references image `{image}`, which is not approved; submit it via prepare_env first (§8.3)"
                )));
            }
        }

        // §3.2: `pre_commands` is the one profile field that is not a *choice*
        // but a *program*. Every other field picks an image, a target, a
        // command line; this one is arbitrary shell that will run inside the
        // sandbox of every other agent that inherits this profile. So it does
        // not travel with the profile — it is split off here, recorded as a
        // request, and only put back by `get_profile` once an administrator has
        // read it. A repository running its own `pre_commands` is untouched:
        // approval gates teaching them to agents that never asked.
        let mut stored_profile = parsed.profile.clone();
        let learned = stored_profile.pre_commands.take().unwrap_or_default();
        if !learned.is_empty() {
            match self.app.store.request_pre_commands(
                &req.project_id,
                &req.path,
                &learned,
                &req.agent_session,
            ) {
                Ok(true) => {
                    self.app
                        .store
                        .audit(
                            &req.agent_session,
                            "pre_commands_requested",
                            &req.project_id,
                            &crate::store::Store::pre_commands_digest(&learned),
                        )
                        .ok();
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "could not record learned pre_commands"),
            }
        }

        let row = crate::store::ProfileRow {
            id: format!("prof-{}", &blake3::hash(format!("{}|{}", req.project_id, req.path).as_bytes()).to_hex()[..16]),
            project_id: req.project_id.clone(),
            path: req.path.clone(),
            adapter: parsed.profile.adapter.clone().unwrap_or_default(),
            image: parsed.profile.image.clone().unwrap_or_default(),
            config_toml: rc_core::profile::to_toml(&stored_profile),
            created_by: req.agent_session.clone(),
            ..Default::default()
        };
        self.app.store.upsert_profile(&row).map_err(internal)?;
        self.app
            .store
            .audit(&req.agent_session, "upsert_profile", &row.id, &req.path)
            .map_err(internal)?;

        let mut message = String::new();
        if !parsed.unknown_keys.is_empty() {
            message = format!("忽略了未知字段: {}", parsed.unknown_keys.join(", "));
        }
        Ok(Response::new(ProfileResp {
            found: true,
            config_toml: req.config_toml,
            message,
            adapter: row.adapter,
            resolved_image: row.image,
            ..Default::default()
        }))
    }

    async fn list_envs(&self, req: Request<ListEnvsReq>) -> Result<Response<ListEnvsResp>, Status> {
        self.authenticate(&req)?;
        let envs = images::list_envs(&self.app, &req.into_inner()).map_err(internal)?;
        Ok(Response::new(ListEnvsResp { envs }))
    }

    async fn prepare_env(&self, req: Request<PrepareEnvReq>) -> Result<Response<EnvStatus>, Status> {
        self.authenticate(&req)?;
        images::prepare_env(&self.app, &req.into_inner())
            .map(Response::new)
            .map_err(|e| Status::invalid_argument(e.to_string()))
    }

    async fn get_env_status(&self, req: Request<EnvQuery>) -> Result<Response<EnvStatus>, Status> {
        self.authenticate(&req)?;
        let id = req.into_inner().env_id;
        match images::resolve_image(&self.app, &id).map_err(internal)? {
            images::ResolveImage::One(row) => Ok(Response::new(images::env_status(&row))),
            images::ResolveImage::Ambiguous(cands) => Err(Status::invalid_argument(format!(
                "ambiguous env ref `{id}`; candidates: {}",
                cands.join(", ")
            ))),
            images::ResolveImage::None => Err(Status::not_found(format!(
                "unknown env `{id}`; use list_envs and pass the env_id= field"
            ))),
        }
    }

    async fn list_workers(&self, req: Request<Empty>) -> Result<Response<ListWorkersResp>, Status> {
        self.authenticate(&req)?;
        let workers = self
            .app
            .workers
            .snapshot()
            .into_iter()
            .map(|w| WorkerBrief {
                worker_id: w.id,
                status: w.status,
                cpu_load: w.stats.cpu_load,
                disk_free_gb: w.stats.disk_free_gb,
                running_tasks: w.stats.running_tasks,
                max_parallel: w.max_parallel,
                arch: w.arch,
                version: w.version,
                last_heartbeat: w.last_hb_ms,
            })
            .collect();
        let counters = self.app.store.overview_counters(3600).map_err(internal)?;
        Ok(Response::new(ListWorkersResp {
            workers,
            queue_depth: counters.queued as u32,
            running: counters.running as u32,
        }))
    }

    /// Cancel a running or pending task. Mirrors the admin path: server writes
    /// the terminal CANCELED state first, then tells the worker to kill.
    ///
    /// Ownership: `project_id` on the request must match the task row. The
    /// bearer token is fleet-wide — this check prevents mis-cancel, not a
    /// malicious agent holding a valid fleet token (R6).
    async fn cancel_task(
        &self,
        req: Request<CancelTaskReq>,
    ) -> Result<Response<CancelTaskResp>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        let task = self
            .app
            .store
            .get_task(&req.task_id)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("unknown task {}", req.task_id)))?;
        if req.project_id.is_empty() || req.project_id != task.project_id {
            return Err(Status::permission_denied(
                "project_id does not match the task; refuse to cancel",
            ));
        }
        if rc_core::TaskState::parse_or_default(&task.status).is_terminal() {
            return Ok(Response::new(CancelTaskResp {
                task_id: req.task_id,
                status: task.status,
                message: "task already finished".into(),
            }));
        }
        // Server-written CANCELED verdict (§5.5); classify never produces this.
        let canceled = TaskResult {
            kind: "infra_error".into(),
            summary: "任务已取消".into(),
            verdict: Some(Verdict {
                status: rc_core::pb::Status::Canceled as i32,
                attribution: Attribution::AttrUnknown as i32,
                rule: "canceled".into(),
                remediation: vec!["任务已由调用方取消".into()],
                evidence: None,
            }),
            ..Default::default()
        };
        // Conditional complete: loses to a concurrent TaskDone (R6).
        let won = self
            .app
            .store
            .complete_task(&req.task_id, "canceled", &canceled, "", "")
            .map_err(internal)?;
        if !won {
            let latest = self
                .app
                .store
                .get_task(&req.task_id)
                .map_err(internal)?
                .map(|t| t.status)
                .unwrap_or_default();
            return Ok(Response::new(CancelTaskResp {
                task_id: req.task_id,
                status: latest,
                message: "task already finished".into(),
            }));
        }
        self.app.progress.lock().remove(&req.task_id);
        self.app.store.unpin_task_blobs(&req.task_id).map_err(internal)?;
        if !task.worker_id.is_empty() {
            self.app
                .workers
                .send(
                    &task.worker_id,
                    ServerCmd {
                        body: Some(server_cmd::Body::CancelTaskId(req.task_id.clone())),
                    },
                )
                .await;
        }
        self.app.publish_task(&req.task_id);
        Ok(Response::new(CancelTaskResp {
            task_id: req.task_id,
            status: "canceled".into(),
            message: "canceled".into(),
        }))
    }
}
