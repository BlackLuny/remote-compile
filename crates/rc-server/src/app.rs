//! Control-plane core: task admission, deduplication, supersede, dispatch and
//! completion. The gRPC and REST layers are thin wrappers over this.

use crate::config::{Config, Policy};
use crate::events::{Event, EventBus};
use crate::metrics::Metrics;
use crate::scheduler::{self, Candidate, Demand};
use crate::store::{Store, TaskRow};
use crate::workers::WorkerRegistry;
use anyhow::{anyhow, Result};
use parking_lot::{Mutex, RwLock};
use rc_core::cas::FsCas;
use rc_core::model::{ResultKind, TaskState, TaskType};
use rc_core::pb;
use rc_core::{ids, manifest, now_ms};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Notify;

pub struct App {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub cas: FsCas,
    pub workers: WorkerRegistry,
    pub events: EventBus,
    pub metrics: Metrics,
    policy: RwLock<Policy>,
    /// Wakes the dispatcher as soon as work or capacity appears, instead of
    /// waiting for the next tick.
    pub dispatch_signal: Notify,
    log_cache: Mutex<HashMap<String, Arc<Vec<String>>>>,
    /// Environment builds already handed to a worker: env_id -> (worker, sent).
    ///
    /// An image row stays `building` for as long as the build runs, which for a
    /// real toolchain image is minutes. The dispatcher ticks every two seconds,
    /// so without this the same order goes out on every tick and the worker
    /// starts another `docker build` each time — the host ends up with hundreds
    /// of concurrent builds. This is runtime state, not a fact about the image,
    /// so it lives here rather than in SQLite: a restart drops the worker
    /// channels too, and re-dispatching then is the correct behaviour.
    building: Mutex<HashMap<String, (String, i64)>>,
}

/// Worker features a task cannot run without.
///
/// A manifest spanning several roots places the repository below the workspace
/// root, and a worker that does not know that would extract the git baseline
/// one directory too high — producing a tree that fails in a way which looks
/// like CAS corruption. Better to leave the task queued and say why.
fn required_capabilities(manifest: Option<&pb::Manifest>) -> Vec<String> {
    match manifest {
        Some(m) if !m.anchor_mount.is_empty() || !m.roots.is_empty() => {
            vec![rc_core::CAP_MULTI_ROOT.to_string()]
        }
        _ => Vec::new(),
    }
}

/// How long a dispatched image build is assumed to still be running. Past this
/// the order goes out again, so that a worker which died mid-build does not
/// strand the environment forever (§8.2).
pub const IMAGE_BUILD_LEASE_SECS: i64 = 3600;

/// Outcome of admitting a submission.
#[derive(Debug)]
pub enum Admission {
    /// Result served from the task cache (§5.1).
    CacheHit { task_id: String, result: pb::TaskResult },
    /// Attached to an identical in-flight task (§5.3).
    Subscribed { task_id: String },
    /// New task queued.
    Queued { task_id: String },
    /// Agent must upload blobs and resubmit (§4.7).
    NeedsBlobs { missing: Vec<String> },
}

impl App {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let store = Arc::new(Store::open(&cfg.db_path())?);
        let cas = FsCas::open(cfg.cas_path())?;
        let policy = Policy::load(&store);

        let requeued = store.reset_inflight_on_boot()?;
        if requeued > 0 {
            tracing::info!(requeued, "re-queued tasks that were in flight before restart");
        }

        Ok(Arc::new(App {
            cfg,
            store,
            cas,
            workers: WorkerRegistry::new(),
            events: EventBus::default(),
            metrics: Metrics::new(),
            policy: RwLock::new(policy),
            dispatch_signal: Notify::new(),
            log_cache: Mutex::new(HashMap::new()),
            building: Mutex::new(HashMap::new()),
        }))
    }

    /// Claim the right to dispatch a build for `env_id`, unless one is already
    /// in flight on a worker that is still online and inside its lease.
    pub fn claim_image_build(&self, env_id: &str, worker: &str) -> bool {
        let now = rc_core::now_secs();
        let mut building = self.building.lock();
        if let Some((holder, sent_at)) = building.get(env_id) {
            let holder_alive = self
                .workers
                .snapshot()
                .iter()
                .any(|w| &w.id == holder && w.status == "online");
            if holder_alive && now - sent_at < IMAGE_BUILD_LEASE_SECS {
                return false;
            }
            tracing::warn!(
                env = %env_id, %holder,
                "image build lease expired or its worker went away; re-dispatching"
            );
        }
        building.insert(env_id.to_string(), (worker.to_string(), now));
        true
    }

    /// Refuse work no connected worker could ever take.
    ///
    /// Queueing it instead would look identical to ordinary congestion: the
    /// agent gets a task id, waits, and is never told that the reason is a
    /// fleet that has not been upgraded.
    fn check_capabilities_available(&self, manifest: &pb::Manifest) -> Result<()> {
        let needed = required_capabilities(Some(manifest));
        if needed.is_empty() {
            return Ok(());
        }
        let online: Vec<_> = self
            .workers
            .snapshot()
            .into_iter()
            .filter(|w| w.status == "online")
            .collect();
        // An empty fleet is ordinary congestion — the machines may be starting,
        // draining or briefly disconnected — and queueing is the right answer.
        // What is worth refusing is a fleet that is up and cannot do the work.
        if online.is_empty() {
            return Ok(());
        }
        for cap in &needed {
            let anyone = online.iter().any(|w| w.capabilities.contains(cap));
            if !anyone {
                return Err(anyhow!(
                    "no online worker supports `{cap}`, which this task needs; \
                     upgrade the compile machines, or set extra_roots = [] to build \
                     without the directories outside the repository"
                ));
            }
        }
        Ok(())
    }

    /// The build finished (either way) — stop holding the slot.
    pub fn release_image_build(&self, env_id: &str) {
        self.building.lock().remove(env_id);
    }

    pub fn policy(&self) -> Policy {
        self.policy.read().clone()
    }

    pub fn set_policy(&self, p: Policy) -> Result<()> {
        p.save(&self.store)?;
        *self.policy.write() = p;
        Ok(())
    }

    // ---------------------------------------------------------------- submit

    pub fn submit(&self, req: &pb::SubmitTaskReq) -> Result<Admission> {
        let policy = self.policy();
        let manifest = req
            .manifest
            .as_ref()
            .ok_or_else(|| anyhow!("submit without a manifest"))?;
        let profile = req
            .profile
            .as_ref()
            .ok_or_else(|| anyhow!("submit without a resolved profile"))?;

        manifest::validate(manifest).map_err(|e| anyhow!("{e}"))?;
        // Both of these end up as path components on the worker (`<mirrors>/
        // <project_id>.git`, `<work>/<worktree_id>`), so their shape is checked
        // here rather than trusted from the wire.
        if !ids::is_valid_project_id(&req.project_id) {
            return Err(anyhow!("malformed project_id: {}", req.project_id));
        }
        if !ids::is_valid_worktree_id(&req.worktree_id) {
            return Err(anyhow!("malformed worktree_id: {}", req.worktree_id));
        }
        self.check_image_admissible(&profile.image, &policy)?;
        self.check_capabilities_available(manifest)?;

        // The server computes the fingerprint itself. Trusting the client's
        // value would let one buggy agent poison every other agent's cache.
        let fingerprint =
            rc_core::fingerprint::compute_for(&manifest.root_hash, profile, &manifest.anchor_mount)
            .map_err(|e| anyhow!("{e}"))?;
        if !req.fingerprint.is_empty() && req.fingerprint != fingerprint {
            tracing::warn!(
                client = %req.fingerprint, server = %fingerprint,
                "fingerprint mismatch; using the server-computed value"
            );
        }

        self.store
            .upsert_project(&req.project_id, &req.repo_url, &req.project_root)?;
        self.store
            .upsert_worktree(&req.worktree_id, &req.project_id, &req.worktree_label)?;
        self.store.note_known_commit(&req.project_id, &manifest.base_commit)?;

        // Blobs the worker will need but the CAS does not hold.
        let needed = manifest::blobs_to_reconcile(manifest);
        self.metrics.incr("blobs_reconciled_total", needed.len() as f64);
        let missing: Vec<String> = needed
            .iter()
            .filter(|h| !self.cas.exists(h))
            .cloned()
            .collect();
        if !missing.is_empty() {
            self.metrics.incr("blobs_missing_total", missing.len() as f64);
            return Ok(Admission::NeedsBlobs { missing });
        }
        if !req.bundle_blob.is_empty() && !self.cas.exists(&req.bundle_blob) {
            return Ok(Admission::NeedsBlobs {
                missing: vec![req.bundle_blob.clone()],
            });
        }

        let task_type = TaskType::parse_or_default(&req.task_type);
        let command = if req.command_override.is_empty() {
            profile
                .tasks
                .get(task_type.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    rc_core::adapter::for_name(&profile.adapter)
                        .command_for(&Default::default(), task_type)
                        .line
                })
        } else {
            req.command_override.clone()
        };

        // §5.1 task cache.
        if !req.no_cache {
            if let Some(prev) = self.store.find_cached_result(&fingerprint, policy.task_cache_ttl_secs)? {
                let id = ids::task_id();
                let row = self.new_row(&id, req, &fingerprint, &command, TaskState::Done);
                self.persist_new(&row, req)?;
                self.store.record_cache_hit(&id, &prev)?;
                self.metrics.incr("tasks_cache_hit_total", 1.0);
                self.store.add_timeline(&id, "cache_hit", "", &prev.id)?;
                self.publish_task(&id);
                let result = prev.result().unwrap_or_default();
                return Ok(Admission::CacheHit { task_id: id, result });
            }
        }

        // §5.3 identical work already in flight: subscribe rather than
        // enqueue a duplicate.
        if let Some(active) = self.store.find_active_by_fingerprint(&fingerprint)? {
            self.store.add_subscriber(&active.id, &req.agent_session)?;
            self.metrics.incr("tasks_dedup_total", 1.0);
            return Ok(Admission::Subscribed { task_id: active.id });
        }

        let id = ids::task_id();
        let row = self.new_row(&id, req, &fingerprint, &command, TaskState::Queued);
        self.persist_new(&row, req)?;
        self.store.add_subscriber(&id, &req.agent_session)?;
        self.store.set_image(&id, &profile.image)?;

        // Pin every blob this task needs until it reaches a terminal state, so
        // GC cannot delete them mid-build (§4.7).
        let mut pinned = needed;
        if !req.bundle_blob.is_empty() {
            pinned.push(req.bundle_blob.clone());
        }
        self.store.pin_task_blobs(&id, &pinned)?;

        self.supersede_older(&row)?;

        self.store.add_timeline(&id, "queued", "", &command)?;
        self.metrics.incr("tasks_submitted_total", 1.0);
        self.publish_task(&id);
        self.dispatch_signal.notify_one();
        Ok(Admission::Queued { task_id: id })
    }

    fn new_row(
        &self,
        id: &str,
        req: &pb::SubmitTaskReq,
        fingerprint: &str,
        command: &str,
        status: TaskState,
    ) -> TaskRow {
        let bytes = req
            .manifest
            .as_ref()
            .map(manifest::dirty_bytes)
            .unwrap_or(0) as i64;
        TaskRow {
            id: id.to_string(),
            task_type: req.task_type.clone(),
            project_id: req.project_id.clone(),
            worktree_id: req.worktree_id.clone(),
            agent_session: req.agent_session.clone(),
            fingerprint: fingerprint.to_string(),
            supersede_key: ids::supersede_key(&req.worktree_id, &req.agent_session, &req.task_type),
            status: status.as_str().to_string(),
            command: command.to_string(),
            created_at: now_ms(),
            bytes_synced: bytes,
            ..Default::default()
        }
    }

    fn persist_new(&self, row: &TaskRow, req: &pb::SubmitTaskReq) -> Result<()> {
        let manifest_json = serde_json::to_string(&req.manifest)?;
        let profile_json = serde_json::to_string(&req.profile)?;
        let base = req
            .manifest
            .as_ref()
            .map(|m| m.base_commit.clone())
            .unwrap_or_default();
        self.store.insert_task(row, &manifest_json, &profile_json, &base)
    }

    /// An image may only run untrusted code once an admin has approved its
    /// digest (§8.3), and the reference must be immutable (§5.1).
    fn check_image_admissible(&self, image: &str, policy: &Policy) -> Result<()> {
        if !rc_core::fingerprint::is_digest_ref(image) {
            return Err(anyhow!(
                "image `{image}` must be pinned to a digest; call get_build_profile / list_envs to resolve one"
            ));
        }
        if !policy.require_image_approval {
            return Ok(());
        }
        let digest = image.split_once('@').map(|(_, d)| d).unwrap_or_default();
        if self.store.is_digest_trusted(digest)? {
            Ok(())
        } else {
            Err(anyhow!(
                "image digest {digest} is not approved yet; an administrator must approve it in the console before it can run code (§8.3)"
            ))
        }
    }

    /// §5.2 — cancel this session's older, not-yet-started tasks of the same
    /// type. A task with subscribers from another session is detached from the
    /// supersede chain instead of cancelled, or those subscribers would wait
    /// forever (risk #23).
    fn supersede_older(&self, incoming: &TaskRow) -> Result<()> {
        let candidates = self
            .store
            .find_supersede_candidates(&incoming.supersede_key, &incoming.id)?;
        for old in candidates {
            let foreign = self.store.foreign_subscribers(&old.id, &incoming.agent_session)?;
            if !foreign.is_empty() {
                self.store.detach_supersede_key(&old.id)?;
                self.store.add_timeline(
                    &old.id,
                    "supersede_skipped",
                    "",
                    &format!("kept for subscribers: {}", foreign.join(",")),
                )?;
                continue;
            }
            self.store.mark_superseded(&old.id, &incoming.id)?;
            self.store.unpin_task_blobs(&old.id)?;
            self.store.add_timeline(&old.id, "superseded", "", &incoming.id)?;
            self.metrics.incr("tasks_superseded_total", 1.0);
            self.publish_task(&old.id);
            if !old.worker_id.is_empty() {
                let workers = self.workers.clone();
                let worker_id = old.worker_id.clone();
                let task_id = old.id.clone();
                tokio::spawn(async move {
                    workers
                        .send(
                            &worker_id,
                            pb::ServerCmd {
                                body: Some(pb::server_cmd::Body::CancelTaskId(task_id)),
                            },
                        )
                        .await;
                });
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- dispatch

    /// Try to place every queued task. Returns how many were dispatched.
    pub async fn dispatch_once(&self) -> usize {
        let policy = self.policy();
        let queued = match self.store.queued_tasks() {
            Ok(q) => q,
            Err(e) => {
                tracing::error!(error = %e, "failed to read the queue");
                return 0;
            }
        };
        self.metrics.set("queue_depth", queued.len() as f64);
        self.metrics.set("running_tasks", self.workers.total_running() as f64);
        self.metrics.set("workers_online", self.workers.online_count() as f64);

        let mut dispatched = 0;
        for task in queued {
            match self.try_dispatch(&task, &policy).await {
                Ok(true) => dispatched += 1,
                Ok(false) => {}
                Err(e) => tracing::error!(task = %task.id, error = %e, "dispatch failed"),
            }
        }
        dispatched
    }

    async fn try_dispatch(&self, task: &TaskRow, policy: &Policy) -> Result<bool> {
        let candidates = self.candidates()?;
        if candidates.is_empty() {
            return Ok(false);
        }
        // Loaded before placement, not after: which worker may run this task
        // depends on what the manifest asks for.
        let Some((manifest_json, profile_json, _base)) = self.store.get_task_inputs(&task.id)? else {
            self.fail_task(&task.id, ResultKind::InfraError, "task inputs are missing")?;
            return Ok(false);
        };
        // Deserialisation failure used to become `None`, which erased the
        // task's capability requirements and let it go to a worker that cannot
        // run it. An unreadable input is an infrastructure failure, not an
        // empty one.
        let manifest: Option<pb::Manifest> = match serde_json::from_str(&manifest_json) {
            Ok(m) => m,
            Err(e) => {
                self.fail_task(&task.id, ResultKind::InfraError, &format!("stored manifest is unreadable: {e}"))?;
                return Ok(false);
            }
        };
        let profile: Option<pb::ResolvedProfile> = match serde_json::from_str(&profile_json) {
            Ok(p) => p,
            Err(e) => {
                self.fail_task(&task.id, ResultKind::InfraError, &format!("stored profile is unreadable: {e}"))?;
                return Ok(false);
            }
        };
        if manifest.is_none() {
            self.fail_task(&task.id, ResultKind::InfraError, "task has no manifest")?;
            return Ok(false);
        }

        let demand = Demand {
            worktree_id: task.worktree_id.clone(),
            project_id: task.project_id.clone(),
            image: task.image.clone(),
            arch: String::new(),
            est_disk_gb: self.estimate_disk_gb(&task.project_id),
            excluded: self.store.attempted_workers(&task.id)?,
            required_capabilities: required_capabilities(manifest.as_ref()),
        };
        let Some(choice) = scheduler::pick(&candidates, &demand, policy) else {
            return Ok(false);
        };

        let assignment = pb::TaskAssignment {
            task_id: task.id.clone(),
            project_id: task.project_id.clone(),
            repo_url: String::new(),
            worktree_id: task.worktree_id.clone(),
            task_type: task.task_type.clone(),
            command: task.command.clone(),
            manifest,
            profile,
            bundle_blobs: self.store.bundles_for(&task.project_id)?,
        };

        self.store.assign_to_worker(&task.id, &choice.worker_id)?;
        self.workers.note_assigned(&choice.worker_id, &task.id);
        // Whatever the agent was asked to re-upload has been re-uploaded, or
        // admission would have refused the resubmission.
        self.clear_missing_blobs(&task.id);
        self.store.add_timeline(
            &task.id,
            "dispatched",
            &choice.worker_id,
            &format!("score {:.2}", choice.score),
        )?;

        let ok = self
            .workers
            .send(
                &choice.worker_id,
                pb::ServerCmd {
                    body: Some(pb::server_cmd::Body::Assign(assignment)),
                },
            )
            .await;
        if !ok {
            self.workers.note_finished(&choice.worker_id, &task.id);
            self.store.requeue(&task.id, &choice.worker_id, "worker channel closed")?;
            return Ok(false);
        }
        self.publish_task(&task.id);
        Ok(true)
    }

    fn candidates(&self) -> Result<Vec<Candidate>> {
        let mut out = Vec::new();
        for w in self.workers.snapshot() {
            out.push(Candidate {
                worker_id: w.id.clone(),
                arch: w.arch.clone(),
                status: w.status.clone(),
                cpu_load: w.stats.cpu_load,
                disk_free_gb: w.stats.disk_free_gb,
                free_slots: w.free_slots(),
                cached_worktrees: w.stats.cached_worktrees.clone(),
                cached_projects: w.stats.cached_projects.clone(),
                cached_images: w.stats.cached_images.clone(),
                busy_worktrees: self.store.busy_worktrees_on_worker(&w.id)?,
                capabilities: w.capabilities.iter().cloned().collect(),
            });
        }
        Ok(out)
    }

    /// Disk estimate from this project's history, with a conservative default
    /// for a project nobody has built yet.
    pub fn estimate_disk_gb(&self, project_id: &str) -> u64 {
        let _ = project_id;
        20
    }

    /// "Why is my task still queued?" — the reason every candidate worker was
    /// rejected. Computed on demand for the console; never on a hot path.
    pub fn explain_placement(&self, task: &TaskRow) -> Vec<(String, String)> {
        let policy = self.policy();
        let Ok(candidates) = self.candidates() else {
            return vec![];
        };
        if candidates.is_empty() {
            return vec![("*".into(), "no worker is connected".into())];
        }
        let demand = Demand {
            worktree_id: task.worktree_id.clone(),
            project_id: task.project_id.clone(),
            image: task.image.clone(),
            arch: String::new(),
            est_disk_gb: self.estimate_disk_gb(&task.project_id),
            excluded: self.store.attempted_workers(&task.id).unwrap_or_default(),
            required_capabilities: self
                .store
                .get_task_inputs(&task.id)
                .ok()
                .flatten()
                .and_then(|(manifest_json, _, _)| serde_json::from_str(&manifest_json).ok())
                .map(|m: Option<pb::Manifest>| required_capabilities(m.as_ref()))
                .unwrap_or_default(),
        };
        scheduler::explain(&candidates, &demand, &policy)
            .into_iter()
            .map(|(worker, reason)| (worker, format!("{reason:?}")))
            .collect()
    }

    // ------------------------------------------------------------ completion

    /// A worker reported a finished task.
    pub async fn on_task_done(&self, worker_id: &str, done: pb::TaskDone) -> Result<()> {
        self.workers.note_finished(worker_id, &done.task_id);
        let Some(task) = self.store.get_task(&done.task_id)? else {
            tracing::warn!(task = %done.task_id, "result for an unknown task");
            return Ok(());
        };
        if TaskState::parse_or_default(&task.status).is_terminal() {
            // Superseded or already completed elsewhere; drop the late result.
            return Ok(());
        }

        // §4.7 self-heal: the worker could not fetch a blob. Ask the agent for
        // it instead of failing the task.
        if !done.missing_blobs.is_empty() {
            self.metrics.incr("blob_missing_selfheal_total", 1.0);
            for h in &done.missing_blobs {
                self.store.forget_blob(h)?;
            }
            self.store.set_status(&done.task_id, TaskState::Syncing.as_str())?;
            self.store.add_timeline(
                &done.task_id,
                "blob_missing",
                worker_id,
                &done.missing_blobs.join(","),
            )?;
            self.store.requeue(&done.task_id, worker_id, "blob_missing")?;
            self.store.set_status(&done.task_id, TaskState::Syncing.as_str())?;
            self.set_missing_blobs(&done.task_id, &done.missing_blobs)?;
            self.publish_task(&done.task_id);
            return Ok(());
        }

        let result = done.result.clone().unwrap_or_default();
        let kind = ResultKind::parse_or_default(&result.kind);
        let policy = self.policy();

        // §6.2: infrastructure failures move to a different machine and are
        // invisible to the agent until the retries run out.
        if kind.is_retryable() && task.attempt < policy.max_infra_retries {
            self.store.requeue(&done.task_id, worker_id, &result.summary)?;
            self.store
                .add_timeline(&done.task_id, "infra_retry", worker_id, &result.summary)?;
            self.metrics.incr("tasks_retried_total", 1.0);
            self.publish_task(&done.task_id);
            self.dispatch_signal.notify_one();
            return Ok(());
        }

        self.finish(&task, &result, &done.log_blob).await
    }

    async fn finish(&self, task: &TaskRow, result: &pb::TaskResult, log_ref: &str) -> Result<()> {
        let kind = ResultKind::parse_or_default(&result.kind);
        let status = if matches!(kind, ResultKind::InfraError) {
            TaskState::Failed
        } else {
            TaskState::Done
        };
        self.store
            .complete_task(&task.id, status.as_str(), result, log_ref, &task.image)?;
        self.store.unpin_task_blobs(&task.id)?;
        self.store.add_timeline(&task.id, "finished", &task.worker_id, &result.kind)?;

        let digest = task.image.split_once('@').map(|(_, d)| d).unwrap_or_default();
        self.store.record_image_outcome(digest, &result.kind)?;
        self.store
            .record_profile_outcome(&task.project_id, "", kind == ResultKind::Success)?;

        self.metrics.incr("tasks_completed_total", 1.0);
        self.metrics.incr(
            match kind {
                ResultKind::Success => "tasks_success_total",
                ResultKind::CompileError => "tasks_compile_error_total",
                ResultKind::EnvError => "tasks_env_error_total",
                ResultKind::InfraError => "tasks_infra_error_total",
                ResultKind::Timeout => "tasks_timeout_total",
            },
            1.0,
        );
        if let Some(stats) = &result.stats {
            self.metrics.observe("task_build_ms", stats.build_ms as f64);
            self.metrics.observe("task_sync_ms", stats.sync_ms as f64);
            self.metrics
                .incr("blob_bytes_uploaded_total", stats.bytes_synced as f64);
        }
        self.publish_task(&task.id);
        self.dispatch_signal.notify_one();
        Ok(())
    }

    pub fn fail_task(&self, task_id: &str, kind: ResultKind, message: &str) -> Result<()> {
        let result = pb::TaskResult {
            kind: kind.as_str().to_string(),
            summary: message.to_string(),
            ..Default::default()
        };
        let status = if kind == ResultKind::InfraError {
            TaskState::Failed
        } else {
            TaskState::Done
        };
        self.store.complete_task(task_id, status.as_str(), &result, "", "")?;
        self.store.unpin_task_blobs(task_id)?;
        self.store.add_timeline(task_id, "failed", "", message)?;
        self.publish_task(task_id);
        Ok(())
    }

    /// A worker vanished; its in-flight tasks go back to the queue.
    pub fn reclaim_worker_tasks(&self, worker_id: &str) -> Result<()> {
        for task in self.store.tasks_on_worker(worker_id)? {
            self.store
                .requeue(&task.id, worker_id, "worker disconnected")?;
            self.store
                .add_timeline(&task.id, "worker_lost", worker_id, "requeued")?;
            self.publish_task(&task.id);
        }
        self.dispatch_signal.notify_one();
        Ok(())
    }

    fn set_missing_blobs(&self, task_id: &str, missing: &[String]) -> Result<()> {
        self.store
            .set_setting(&format!("missing_blobs:{task_id}"), &serde_json::to_string(missing)?)
    }

    pub fn missing_blobs(&self, task_id: &str) -> Vec<String> {
        self.store
            .get_setting(&format!("missing_blobs:{task_id}"))
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn clear_missing_blobs(&self, task_id: &str) {
        let _ = self.store.set_setting(&format!("missing_blobs:{task_id}"), "[]");
    }

    // ------------------------------------------------------------------ logs

    /// Paged log access (§11 L2). There is deliberately no "give me
    /// everything" path — a full build log is tens of thousands of lines.
    pub fn get_log(&self, q: &pb::LogQuery) -> Result<pb::LogChunk> {
        let Some(task) = self.store.get_task(&q.task_id)? else {
            return Err(anyhow!("unknown task {}", q.task_id));
        };
        if task.log_ref.is_empty() {
            return Ok(pb::LogChunk {
                lines: vec![],
                offset: 0,
                total_lines: 0,
                truncated: false,
            });
        }
        let lines = self.log_lines(&task.log_ref)?;
        let filtered: Vec<&String> = if q.grep.is_empty() {
            lines.iter().collect()
        } else {
            let needle = q.grep.to_lowercase();
            lines
                .iter()
                .filter(|l| l.to_lowercase().contains(&needle))
                .collect()
        };
        let total = filtered.len() as u64;
        let limit = if q.limit == 0 { 200 } else { q.limit.min(2000) } as usize;
        let start = if q.tail {
            filtered.len().saturating_sub(limit)
        } else {
            (q.offset as usize).min(filtered.len())
        };
        let end = (start + limit).min(filtered.len());
        Ok(pb::LogChunk {
            lines: filtered[start..end].iter().map(|s| (*s).clone()).collect(),
            offset: start as u64,
            total_lines: total,
            truncated: end < filtered.len(),
        })
    }

    fn log_lines(&self, log_ref: &str) -> Result<Arc<Vec<String>>> {
        if let Some(hit) = self.log_cache.lock().get(log_ref) {
            return Ok(hit.clone());
        }
        let raw = self.cas.get(log_ref)?;
        let text = rc_core::cas::decompress_log(&raw)
            .unwrap_or_else(|_| String::from_utf8_lossy(&raw).into_owned());
        let lines: Arc<Vec<String>> = Arc::new(text.lines().map(|l| l.to_string()).collect());
        let mut cache = self.log_cache.lock();
        // Bounded: build logs are big and the viewer only reads a few.
        if cache.len() > 16 {
            cache.clear();
        }
        cache.insert(log_ref.to_string(), lines.clone());
        Ok(lines)
    }

    // ---------------------------------------------------------------- events

    pub fn publish_task(&self, task_id: &str) {
        if let Ok(Some(t)) = self.store.get_task(task_id) {
            self.events.publish(Event::TaskUpdated {
                task_id: t.id,
                status: t.status,
                result_kind: t.result_kind,
                task_type: t.task_type,
                project_id: t.project_id,
                worktree_id: t.worktree_id,
                worker_id: t.worker_id,
                at: now_ms(),
            });
        }
    }

    pub fn task_status(&self, task_id: &str) -> Result<Option<pb::TaskStatus>> {
        let Some(t) = self.store.get_task(task_id)? else {
            return Ok(None);
        };
        Ok(Some(pb::TaskStatus {
            task_id: t.id.clone(),
            status: t.status.clone(),
            result: t.result(),
            timeline: self.store.timeline(&t.id)?,
            attempt: t.attempt as u32,
            worker_id: t.worker_id.clone(),
            created_at: t.created_at,
            finished_at: t.finished_at,
            missing_blobs: self.missing_blobs(&t.id),
            superseded_by: t.superseded_by.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::pb::{EntryType, FileEntry};

    fn test_app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("rc-app-{}", ulid::Ulid::generate()));
        let app = App::new(Config {
            data_dir: dir,
            http_addr: "127.0.0.1:0".into(),
            grpc_addr: "127.0.0.1:0".into(),
            allow_anonymous_agents: true,
            session_ttl_secs: 3600,
        })
        .unwrap();
        // Approve the image the fixtures use.
        app.store
            .upsert_image(&crate::store::ImageRow {
                id: "e1".into(),
                digest: DIGEST.into(),
                status: "healthy".into(),
                ..Default::default()
            })
            .unwrap();
        app.store.approve_image("e1", "test").unwrap();
        app
    }

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    // Well-formed on purpose: submit refuses ids that could escape a directory
    // once the worker joins them into a path.
    const TEST_PROJECT: &str = "p-0123456789abcdef";
    const TEST_WORKTREE: &str = "w-0123456789abcdef";

    fn image_ref() -> String {
        format!("reg/env/rust@{DIGEST}")
    }

    fn request(app: &App, session: &str, task_type: &str, content: &[u8]) -> pb::SubmitTaskReq {
        let hash = app.cas.put(content).unwrap();
        let m = manifest::build(
            vec![FileEntry {
                path: "src/main.rs".into(),
                size: content.len() as u64,
                hash,
                r#type: EntryType::EntryFile as i32,
                executable: false,
                in_baseline: false,
                symlink_target: String::new(),
            }],
            "",
            false,
        );
        let profile = pb::ResolvedProfile {
            adapter: "rust".into(),
            image: image_ref(),
            canonical: format!("task_type={task_type}\ncommand=cargo {task_type}\n"),
            toolchain: "rustc 1.85.0".into(),
            ..Default::default()
        };
        pb::SubmitTaskReq {
            project_id: TEST_PROJECT.into(),
            worktree_id: TEST_WORKTREE.into(),
            agent_session: session.into(),
            task_type: task_type.into(),
            manifest: Some(m),
            profile: Some(profile),
            ..Default::default()
        }
    }

    fn queued_id(a: Admission) -> String {
        match a {
            Admission::Queued { task_id } => task_id,
            other => panic!(
                "expected Queued, got {}",
                match other {
                    Admission::CacheHit { .. } => "CacheHit",
                    Admission::Subscribed { .. } => "Subscribed",
                    Admission::NeedsBlobs { .. } => "NeedsBlobs",
                    Admission::Queued { .. } => unreachable!(),
                }
            ),
        }
    }

    #[test]
    fn a_submission_with_uploaded_blobs_is_queued() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"fn main(){}")).unwrap());
        assert_eq!(app.store.get_task(&id).unwrap().unwrap().status, "queued");
    }

    #[test]
    fn missing_blobs_are_requested_instead_of_queuing() {
        let app = test_app();
        let mut req = request(&app, "s1", "check", b"content");
        // Point the manifest at a blob nobody uploaded.
        let m = req.manifest.as_mut().unwrap();
        m.entries[0].hash = "b".repeat(64);
        m.root_hash = manifest::root_hash(&m.entries);
        match app.submit(&req).unwrap() {
            Admission::NeedsBlobs { missing } => assert_eq!(missing, vec!["b".repeat(64)]),
            _ => panic!("expected NeedsBlobs"),
        }
    }

    #[test]
    fn an_unapproved_image_cannot_run_code() {
        // §8.3: image build is its own attack surface.
        let app = test_app();
        let mut req = request(&app, "s1", "check", b"x");
        req.profile.as_mut().unwrap().image =
            "reg/evil@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        let err = app.submit(&req).unwrap_err().to_string();
        assert!(err.contains("not approved"), "{err}");
    }

    #[test]
    fn multi_root_is_only_required_when_the_layout_is_actually_multi_root() {
        // Every ordinary project must stay runnable on every worker.
        assert!(required_capabilities(None).is_empty());
        assert!(required_capabilities(Some(&pb::Manifest::default())).is_empty());

        let nested = pb::Manifest {
            anchor_mount: "app".into(),
            ..Default::default()
        };
        assert_eq!(required_capabilities(Some(&nested)), vec!["multi-root"]);
    }

    #[test]
    fn an_empty_fleet_queues_but_an_incapable_one_refuses() {
        // "No workers at all" is congestion and resolves itself. "Workers that
        // cannot do this" never does, and queueing there looks identical to the
        // agent while never completing.
        let app = test_app();
        let multi = pb::Manifest {
            anchor_mount: "app".into(),
            ..Default::default()
        };
        assert!(
            app.check_capabilities_available(&multi).is_ok(),
            "an empty fleet must not turn into a hard failure"
        );

        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        app.workers.connect("w-old", "x86_64", "0.1.0", 4, tx);
        let err = app.check_capabilities_available(&multi).unwrap_err();
        assert!(err.to_string().contains("multi-root"), "{err}");
        // A plain task is unaffected.
        assert!(app.check_capabilities_available(&pb::Manifest::default()).is_ok());
    }

    #[test]
    fn a_mutable_tag_is_refused() {
        // §5.1: a tag can be repointed, which would poison the task cache.
        let app = test_app();
        let mut req = request(&app, "s1", "check", b"x");
        req.profile.as_mut().unwrap().image = "reg/env/rust:latest".into();
        let err = app.submit(&req).unwrap_err().to_string();
        assert!(err.contains("digest"), "{err}");
    }

    #[test]
    fn a_forged_manifest_hash_is_rejected() {
        let app = test_app();
        let mut req = request(&app, "s1", "check", b"x");
        req.manifest.as_mut().unwrap().root_hash = "deadbeef".into();
        assert!(app.submit(&req).is_err());
    }

    #[test]
    fn identical_work_from_another_session_subscribes() {
        let app = test_app();
        let first = queued_id(app.submit(&request(&app, "s1", "check", b"same")).unwrap());
        match app.submit(&request(&app, "s2", "check", b"same")).unwrap() {
            Admission::Subscribed { task_id } => assert_eq!(task_id, first),
            _ => panic!("expected Subscribed"),
        }
    }

    #[test]
    fn a_completed_result_is_served_from_cache() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"cached")).unwrap());
        app.store
            .complete_task(
                &id,
                "done",
                &pb::TaskResult { kind: "success".into(), ..Default::default() },
                "",
                "",
            )
            .unwrap();
        match app.submit(&request(&app, "s1", "check", b"cached")).unwrap() {
            Admission::CacheHit { result, task_id } => {
                assert_eq!(result.kind, "success");
                assert_ne!(task_id, id, "a cache hit still gets its own task id");
            }
            _ => panic!("expected CacheHit"),
        }
    }

    #[test]
    fn an_infra_failure_is_never_replayed_from_cache() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"infra")).unwrap());
        app.store
            .complete_task(
                &id,
                "failed",
                &pb::TaskResult { kind: "infra_error".into(), ..Default::default() },
                "",
                "",
            )
            .unwrap();
        assert!(matches!(
            app.submit(&request(&app, "s1", "check", b"infra")).unwrap(),
            Admission::Queued { .. }
        ));
    }

    #[test]
    fn no_cache_forces_a_fresh_run() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"c")).unwrap());
        app.store
            .complete_task(&id, "done", &pb::TaskResult { kind: "success".into(), ..Default::default() }, "", "")
            .unwrap();
        let mut req = request(&app, "s1", "check", b"c");
        req.no_cache = true;
        assert!(matches!(app.submit(&req).unwrap(), Admission::Queued { .. }));
    }

    #[test]
    fn newer_code_supersedes_the_same_sessions_queued_check() {
        let app = test_app();
        let old = queued_id(app.submit(&request(&app, "s1", "check", b"v1")).unwrap());
        let new = queued_id(app.submit(&request(&app, "s1", "check", b"v2")).unwrap());
        assert_eq!(app.store.get_task(&old).unwrap().unwrap().status, "superseded");
        assert_eq!(app.store.get_task(&old).unwrap().unwrap().superseded_by, new);
    }

    #[test]
    fn clippy_does_not_cancel_a_queued_check() {
        // Risk #22: supersede scope includes the task type.
        let app = test_app();
        let check = queued_id(app.submit(&request(&app, "s1", "check", b"v1")).unwrap());
        queued_id(app.submit(&request(&app, "s1", "clippy", b"v1")).unwrap());
        assert_eq!(app.store.get_task(&check).unwrap().unwrap().status, "queued");
    }

    #[test]
    fn one_agent_cannot_supersede_another_in_a_shared_worktree() {
        // §5.2: two agents in one worktree must not cancel each other.
        let app = test_app();
        let a = queued_id(app.submit(&request(&app, "s1", "check", b"v1")).unwrap());
        queued_id(app.submit(&request(&app, "s2", "check", b"v2")).unwrap());
        assert_eq!(app.store.get_task(&a).unwrap().unwrap().status, "queued");
    }

    #[test]
    fn a_task_with_foreign_subscribers_survives_supersede() {
        // Risk #23: cancelling it would leave the subscriber pending forever.
        let app = test_app();
        let shared = queued_id(app.submit(&request(&app, "s1", "check", b"v1")).unwrap());
        // s2 attaches to the same fingerprint.
        assert!(matches!(
            app.submit(&request(&app, "s2", "check", b"v1")).unwrap(),
            Admission::Subscribed { .. }
        ));
        // s1 moves on to newer code.
        queued_id(app.submit(&request(&app, "s1", "check", b"v2")).unwrap());

        let kept = app.store.get_task(&shared).unwrap().unwrap();
        assert_eq!(kept.status, "queued", "subscriber would hang if this were cancelled");
        assert_eq!(kept.supersede_key, "", "but it is detached from the supersede chain");
    }

    #[test]
    fn superseding_releases_the_blob_pins() {
        let app = test_app();
        let old = queued_id(app.submit(&request(&app, "s1", "check", b"v1")).unwrap());
        assert!(app.store.collectable_blobs(-1, 10).unwrap().is_empty());
        queued_id(app.submit(&request(&app, "s1", "check", b"v2")).unwrap());
        let freed: Vec<String> = app
            .store
            .collectable_blobs(-1, 10)
            .unwrap()
            .into_iter()
            .map(|b| b.hash)
            .collect();
        assert_eq!(freed.len(), 1, "the superseded task's blob is collectable again");
        assert_eq!(app.store.get_task(&old).unwrap().unwrap().status, "superseded");
    }

    #[tokio::test]
    async fn infra_errors_retry_before_reaching_the_agent() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"x")).unwrap());
        app.store.assign_to_worker(&id, "w-a").unwrap();

        let done = pb::TaskDone {
            task_id: id.clone(),
            result: Some(pb::TaskResult {
                kind: "infra_error".into(),
                summary: "docker daemon unreachable".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        app.on_task_done("w-a", done.clone()).await.unwrap();
        let t = app.store.get_task(&id).unwrap().unwrap();
        assert_eq!(t.status, "queued", "retried instead of reported");
        assert_eq!(app.store.attempted_workers(&id).unwrap(), vec!["w-a"]);
    }

    #[tokio::test]
    async fn infra_errors_surface_once_retries_are_exhausted() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"x")).unwrap());
        let done = |t: &str| pb::TaskDone {
            task_id: t.to_string(),
            result: Some(pb::TaskResult { kind: "infra_error".into(), ..Default::default() }),
            ..Default::default()
        };
        for w in ["w-a", "w-b", "w-c"] {
            app.store.assign_to_worker(&id, w).unwrap();
            app.on_task_done(w, done(&id)).await.unwrap();
        }
        let t = app.store.get_task(&id).unwrap().unwrap();
        assert_eq!(t.status, "failed");
        assert_eq!(t.result_kind, "infra_error");
    }

    #[tokio::test]
    async fn a_blob_missing_report_triggers_a_resync_not_a_failure() {
        // §4.7: GC raced the worker; the agent re-uploads and nobody is told
        // their code is broken.
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"x")).unwrap());
        app.store.assign_to_worker(&id, "w-a").unwrap();
        let done = pb::TaskDone {
            task_id: id.clone(),
            missing_blobs: vec!["c".repeat(64)],
            ..Default::default()
        };
        app.on_task_done("w-a", done).await.unwrap();
        assert_eq!(app.store.get_task(&id).unwrap().unwrap().status, "syncing");
        assert_eq!(app.missing_blobs(&id), vec!["c".repeat(64)]);
    }

    #[tokio::test]
    async fn a_late_result_for_a_superseded_task_is_ignored() {
        let app = test_app();
        let old = queued_id(app.submit(&request(&app, "s1", "check", b"v1")).unwrap());
        queued_id(app.submit(&request(&app, "s1", "check", b"v2")).unwrap());
        app.on_task_done(
            "w-a",
            pb::TaskDone {
                task_id: old.clone(),
                result: Some(pb::TaskResult { kind: "success".into(), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(app.store.get_task(&old).unwrap().unwrap().status, "superseded");
    }

    #[tokio::test]
    async fn completion_unpins_blobs_and_records_image_health() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"x")).unwrap());
        app.store.assign_to_worker(&id, "w-a").unwrap();
        app.on_task_done(
            "w-a",
            pb::TaskDone {
                task_id: id.clone(),
                result: Some(pb::TaskResult { kind: "success".into(), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(app.store.get_task(&id).unwrap().unwrap().status, "done");
        assert_eq!(app.store.collectable_blobs(-1, 10).unwrap().len(), 1);
        assert_eq!(app.store.get_image("e1").unwrap().unwrap().success_count, 1);
    }

    #[test]
    fn log_paging_supports_offset_grep_and_tail() {
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"x")).unwrap());
        let text: String = (0..100)
            .map(|i| if i % 10 == 0 { format!("error line {i}\n") } else { format!("line {i}\n") })
            .collect();
        let blob = app
            .cas
            .put(&rc_core::cas::compress_log(&text).unwrap())
            .unwrap();
        app.store
            .complete_task(&id, "done", &pb::TaskResult::default(), &blob, "")
            .unwrap();

        let page = app
            .get_log(&pb::LogQuery { task_id: id.clone(), offset: 0, limit: 5, ..Default::default() })
            .unwrap();
        assert_eq!(page.lines.len(), 5);
        assert_eq!(page.lines[0], "error line 0");
        assert_eq!(page.lines[1], "line 1");
        assert_eq!(page.total_lines, 100);
        assert!(page.truncated);

        let tail = app
            .get_log(&pb::LogQuery { task_id: id.clone(), limit: 3, tail: true, ..Default::default() })
            .unwrap();
        assert_eq!(tail.lines.last().unwrap(), "line 99");
        assert_eq!(tail.lines.len(), 3);

        let grepped = app
            .get_log(&pb::LogQuery { task_id: id, grep: "error".into(), limit: 100, ..Default::default() })
            .unwrap();
        assert_eq!(grepped.lines.len(), 10);
    }

    #[test]
    fn logs_are_never_returned_wholesale_by_default() {
        // §11: an unbounded log dump would blow up an agent's context.
        let app = test_app();
        let id = queued_id(app.submit(&request(&app, "s1", "check", b"x")).unwrap());
        let text: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let blob = app.cas.put(&rc_core::cas::compress_log(&text).unwrap()).unwrap();
        app.store
            .complete_task(&id, "done", &pb::TaskResult::default(), &blob, "")
            .unwrap();
        let page = app
            .get_log(&pb::LogQuery { task_id: id, limit: 0, ..Default::default() })
            .unwrap();
        assert_eq!(page.lines.len(), 200, "an unset limit still pages");
        assert!(page.truncated);
    }
}
