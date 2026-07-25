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
        match self.app.submit(&req) {
            Ok(Admission::Queued { task_id }) => Ok(Response::new(TaskHandle {
                task_id,
                status: "queued".into(),
                ..Default::default()
            })),
            Ok(Admission::Subscribed { task_id }) => Ok(Response::new(TaskHandle {
                task_id,
                status: "queued".into(),
                subscribed: true,
                message: "identical work already in flight; attached to it".into(),
                ..Default::default()
            })),
            Ok(Admission::CacheHit { task_id, result }) => Ok(Response::new(TaskHandle {
                task_id,
                status: "done".into(),
                cache_hit: true,
                result: Some(result),
                ..Default::default()
            })),
            Ok(Admission::NeedsBlobs { missing }) => Ok(Response::new(TaskHandle {
                status: "needs_blobs".into(),
                missing_blobs: missing,
                message: "upload the listed blobs and resubmit".into(),
                ..Default::default()
            })),
            Err(e) => Err(Status::failed_precondition(e.to_string())),
        }
    }

    /// Long-poll: return as soon as the task reaches a terminal state, or when
    /// `wait_secs` elapses. A short wait turns most incremental checks into a
    /// synchronous call (§12).
    async fn get_task(&self, req: Request<TaskQuery>) -> Result<Response<TaskStatus>, Status> {
        self.authenticate(&req)?;
        let req = req.into_inner();
        let deadline = std::time::Duration::from_secs(req.wait_secs.min(60) as u64);
        let mut rx = self.app.events.subscribe();

        let current = self
            .app
            .task_status(&req.task_id)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("unknown task {}", req.task_id)))?;
        if req.wait_secs == 0 || rc_core::TaskState::parse_or_default(&current.status).is_terminal() {
            return Ok(Response::new(current));
        }

        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.task_id() == Some(req.task_id.as_str()) => {
                        if let Ok(Some(st)) = self.app.task_status(&req.task_id) {
                            if rc_core::TaskState::parse_or_default(&st.status).is_terminal() {
                                return Some(st);
                            }
                        }
                    }
                    Ok(_) => {}
                    // Lagged: fall back to a direct read rather than giving up.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Ok(Some(st)) = self.app.task_status(&req.task_id) {
                            if rc_core::TaskState::parse_or_default(&st.status).is_terminal() {
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
                    .task_status(&req.task_id)
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

        let message = if resolved.is_empty() {
            "控制面还没有已审批的可用镜像：用 prepare_env 提交 Dockerfile，管理员审批后即可使用（§8.3/§8.4）".to_string()
        } else if stored.is_none() {
            "该项目还没有 Build Profile，返回的是 fleet 默认镜像；首次成功后会自动沉淀".to_string()
        } else {
            String::new()
        };

        Ok(Response::new(ProfileResp {
            found: stored.is_some(),
            config_toml: stored.as_ref().map(|p| p.config_toml.clone()).unwrap_or_default(),
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

        let row = crate::store::ProfileRow {
            id: format!("prof-{}", &blake3::hash(format!("{}|{}", req.project_id, req.path).as_bytes()).to_hex()[..16]),
            project_id: req.project_id.clone(),
            path: req.path.clone(),
            adapter: parsed.profile.adapter.clone().unwrap_or_default(),
            image: parsed.profile.image.clone().unwrap_or_default(),
            config_toml: req.config_toml.clone(),
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
        match self.app.store.get_image(&id).map_err(internal)? {
            Some(row) => Ok(Response::new(images::env_status(&row))),
            None => Err(Status::not_found(format!("unknown env {id}"))),
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
}
