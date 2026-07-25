//! Task execution: rebuild the workspace, run the build in a sandbox, turn
//! the output into a structured result.

use crate::client::{self, ServerClient};
use crate::config::WorkerConfig;
use crate::docker::{self, Network, RunSpec, Sandbox};
use crate::gitmirror::{BaselineSource, Mirror};
use crate::workspace;
use anyhow::{anyhow, Result};
use rc_core::cas::FsCas;
use rc_core::model::TaskType;
use rc_core::pb::{self, TaskAssignment, TaskDone, TaskResult, TaskStats};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Mutual exclusion keyed by an arbitrary id, one lock per distinct key.
#[derive(Default)]
pub struct KeyedLocks {
    inner: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl KeyedLocks {
    pub async fn get(&self, key: &str) -> Arc<Mutex<()>> {
        let mut map = self.inner.lock().await;
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

pub struct Runner {
    pub cfg: WorkerConfig,
    pub sandbox: Arc<Sandbox>,
    pub cas: FsCas,
    /// Proxy address inside the build network, if egress is enabled.
    pub proxy: Option<String>,
    /// Only one task per worktree runs at a time on this worker: they share a
    /// target volume and would otherwise just block on cargo's file lock
    /// (§6.2, risk #8).
    worktree_locks: KeyedLocks,
    /// Git mirrors are keyed by *project*, so two different worktrees of one
    /// project — which the worktree lock deliberately lets run in parallel —
    /// drive the same bare repository at once. `Mirror::open`'s "is HEAD there,
    /// else init" and `ensure_commit`'s fetch/import/check sequence are both
    /// multi-step and neither is atomic, so concurrent tasks can collide on
    /// git's ref locks and report the baseline as unavailable. That is not
    /// fatal — it degrades to full L2 — but it is slow and it looks like a bug.
    ///
    /// This covers one worker process, which is the situation that actually
    /// arises; two rc-worker processes sharing a data dir would still need a
    /// lockfile.
    project_locks: KeyedLocks,
    /// Whether a host sccache daemon is available for the UDS mount (§7.2).
    pub sccache_available: bool,
    /// Environment builds currently running here. A duplicate order for one
    /// already in flight is dropped rather than started again: a `docker build`
    /// of a toolchain image saturates the host, and nothing above guarantees an
    /// order arrives only once.
    building: Arc<Mutex<HashSet<String>>>,
}

impl Runner {
    pub fn new(cfg: WorkerConfig, sandbox: Arc<Sandbox>, proxy: Option<String>) -> Result<Self> {
        let cas = FsCas::open(cfg.cas_dir())?;
        let sccache_available = sccache_enabled();
        Ok(Runner {
            cfg,
            sandbox,
            cas,
            proxy,
            worktree_locks: KeyedLocks::default(),
            project_locks: KeyedLocks::default(),
            sccache_available,
            building: Arc::new(Mutex::new(HashSet::new())),
        })
    }


    pub async fn execute(
        &self,
        assignment: TaskAssignment,
        client: &mut ServerClient,
        events: mpsc::Sender<pb::WorkerEvent>,
    ) -> TaskDone {
        let task_id = assignment.task_id.clone();
        let lock = self.worktree_locks.get(&assignment.worktree_id).await;
        let _guard = lock.lock().await;

        match self.execute_inner(assignment, client, &events).await {
            Ok(done) => done,
            // Anything that escapes here is our problem, not the code's: the
            // control plane retries it on another machine (§3.5).
            Err(e) => {
                tracing::error!(task = %task_id, error = %e, "task failed with an infrastructure error");
                client::infra_failure(&task_id, e)
            }
        }
    }

    async fn execute_inner(
        &self,
        assignment: TaskAssignment,
        client: &mut ServerClient,
        events: &mpsc::Sender<pb::WorkerEvent>,
    ) -> Result<TaskDone> {
        let task_id = assignment.task_id.clone();
        let manifest = assignment
            .manifest
            .clone()
            .ok_or_else(|| anyhow!("assignment has no manifest"))?;
        let profile = assignment
            .profile
            .clone()
            .ok_or_else(|| anyhow!("assignment has no resolved profile"))?;
        rc_core::manifest::validate(&manifest).map_err(|e| anyhow!(e))?;
        // The server checks these too; repeating it here is deliberate, because
        // this is where they become paths and volume names, and a worker must
        // not depend on the control plane having been careful.
        if !rc_core::ids::is_valid_project_id(&assignment.project_id) {
            return Err(anyhow!("malformed project_id: {}", assignment.project_id));
        }
        if !rc_core::ids::is_valid_worktree_id(&assignment.worktree_id) {
            return Err(anyhow!("malformed worktree_id: {}", assignment.worktree_id));
        }

        let sync_started = std::time::Instant::now();
        let _ = events
            .send(client::progress_event(&task_id, "syncing", "rebuilding workspace"))
            .await;

        let root = docker::workspace_dir(&self.cfg.work_dir(), &assignment.worktree_id);
        std::fs::create_dir_all(&root)?;

        // ---- L1 baseline ----
        let mut baseline_note = String::new();
        if manifest.baseline && !manifest.base_commit.is_empty() {
            let bundles = self
                .materialize_bundles(&assignment.bundle_blobs, client)
                .await?;
            // Everything touching this project's bare mirror is serialized.
            let project_lock = self.project_locks.get(&assignment.project_id).await;
            let _mirror_guard = project_lock.lock().await;

            let mirror = Mirror::open(&self.cfg.mirror_dir(), &assignment.project_id)?;
            let source = mirror.ensure_commit(
                &manifest.base_commit,
                Some(&assignment.repo_url).filter(|u| !u.is_empty()).map(|s| s.as_str()),
                &bundles,
            );
            baseline_note = format!("{source:?}");
            if source != BaselineSource::Unavailable {
                if let Err(e) = mirror.extract(&manifest.base_commit, &root) {
                    // Not fatal: the manifest still describes every file, so
                    // the L2 layer can supply all of them (§4.1 step 4).
                    tracing::warn!(error = %e, "baseline extraction failed; falling back to full L2");
                    baseline_note = format!("{source:?}+extract_failed");
                }
            }
        }

        // ---- L2 content-addressed layer ----
        let plan = workspace::plan(&root, &manifest)?;

        // Deletions come first, before anything is written. The previous task's
        // build could have replaced a directory with a symlink pointing outside
        // the workspace; writing into it before clearing it would follow that
        // link. §7.3 wants these strays gone regardless, so the only question
        // was ordering — and "after the writes" was the wrong answer.
        workspace::apply_deletions(&root, &plan)?;

        let mut missing = Vec::new();
        let mut bytes_synced = 0u64;
        for entry in &plan.fetch {
            let data = match self.blob(&entry.hash, client).await? {
                Some(d) => d,
                None => {
                    missing.push(entry.hash.clone());
                    continue;
                }
            };
            bytes_synced += data.len() as u64;
            workspace::write_file(&root, entry, &data)?;
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            tracing::warn!(task = %task_id, count = missing.len(), "blobs missing from the CAS");
            return Ok(client::blob_missing(&task_id, missing));
        }
        for (path, target) in &plan.symlinks {
            workspace::write_symlink(&root, path, target)?;
        }
        // §7.3: the manifest is the whole truth, and `verify` is what says so.
        workspace::verify(&root, &manifest)?;
        let sync_ms = sync_started.elapsed().as_millis() as u64;

        // ---- execute ----
        let _ = events
            .send(client::progress_event(
                &task_id,
                "building",
                &format!("baseline={baseline_note} synced={bytes_synced}B"),
            ))
            .await;

        let build_started = std::time::Instant::now();
        let output = self.run_build(&assignment, &profile, &root).await?;
        let build_ms = build_started.elapsed().as_millis() as u64;

        // ---- interpret ----
        let adapter = rc_core::adapter::for_name(&profile.adapter);
        let diagnostics = adapter.parse_diagnostics(&output.stdout, &output.stderr);
        let task_type = TaskType::parse_or_default(&assignment.task_type);
        let combined = output.combined();
        let classification = rc_core::diag::classify(
            task_type,
            output.exit_code,
            output.timed_out,
            &diagnostics,
            &combined,
        );
        let (top, truncated) = rc_core::diag::top_diagnostics(&diagnostics, 50);

        let _ = events
            .send(client::progress_event(&task_id, "uploading", "storing build log"))
            .await;
        let log_blob = match rc_core::cas::compress_log(&combined) {
            Ok(packed) => client.put_blob(packed).await.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "failed to compress the build log");
                String::new()
            }
        };

        Ok(TaskDone {
            task_id,
            result: Some(TaskResult {
                kind: classification.kind.as_str().to_string(),
                diagnostics: top,
                error_count: classification.error_count,
                warning_count: classification.warning_count,
                log_ref: log_blob.clone(),
                stats: Some(TaskStats {
                    queue_ms: 0,
                    sync_ms,
                    build_ms,
                    upload_ms: 0,
                    cache_hit_rate: 0.0,
                    bytes_synced,
                }),
                summary: classification.summary,
                exit_code: output.exit_code,
                truncated_diagnostics: truncated,
            }),
            log_blob,
            log_lines: combined.lines().count() as u64,
            missing_blobs: vec![],
        })
    }

    async fn run_build(
        &self,
        assignment: &TaskAssignment,
        profile: &pb::ResolvedProfile,
        root: &Path,
    ) -> Result<docker::RunOutput> {
        let adapter = rc_core::adapter::for_name(&profile.adapter);
        let cache = adapter.cache_config(profile);

        let image = self.sandbox.ensure_image(&profile.image).await?;

        let mut volumes = Vec::new();
        let labels = HashMap::from([
            (docker::LABEL_TASK.to_string(), assignment.task_id.clone()),
            (docker::LABEL_WORKTREE.to_string(), assignment.worktree_id.clone()),
            (docker::LABEL_PROJECT.to_string(), assignment.project_id.clone()),
        ]);
        if let Some(mount) = &cache.target_mount {
            let name = docker::target_volume(&assignment.worktree_id);
            self.sandbox.ensure_volume(&name, labels.clone()).await?;
            volumes.push((name, mount.clone()));
        }
        if let Some(mount) = &cache.rustup_mount {
            // Worker-wide, unlabelled by project: a toolchain is the same bytes
            // whoever asked for it, and GC keys off project/worktree labels.
            let name = docker::rustup_volume();
            self.sandbox
                .ensure_volume(&name, docker::Sandbox::base_labels())
                .await?;
            volumes.push((name, mount.clone()));
        }
        if let Some(mount) = &cache.registry_mount {
            let name = docker::registry_volume(&assignment.project_id);
            let mut vol_labels = docker::Sandbox::base_labels();
            vol_labels.insert(docker::LABEL_PROJECT.to_string(), assignment.project_id.clone());
            self.sandbox.ensure_volume(&name, vol_labels).await?;
            volumes.push((name, mount.clone()));
        }

        let mut binds = Vec::new();
        let mut env: Vec<String> = Vec::new();
        for (k, v) in &cache.env {
            // §7.2: without a host sccache daemon, RUSTC_WRAPPER=sccache would
            // fail every compile — worse than having no cache at all.
            if k == "RUSTC_WRAPPER" && !(self.sccache_available && cache.sccache_uds) {
                continue;
            }
            env.push(format!("{k}={v}"));
        }
        if cache.sccache_uds && self.sccache_available {
            binds.push((self.cfg.sccache_dir(), "/rc/sccache".to_string(), false));
        }
        if let Some(target) = &cache.target_mount {
            env.push(format!("CARGO_TARGET_DIR={target}"));
        }
        env.push("HOME=/rc/home".to_string());
        env.push("TERM=dumb".to_string());

        // A sub-project inside a monorepo builds from its own directory inside
        // the mounted workspace (§3.2). The path comes from the agent, so it is
        // checked rather than trusted: `../..` here would put the build's cwd
        // outside the workspace entirely.
        let workdir = subproject_workdir(&profile.path)?;

        // pre_commands generate code, so they run inside the same sandbox and
        // are part of the fingerprint (§5.1).
        let mut script = String::from("set -e\n");
        for cmd in &profile.pre_commands {
            script.push_str(cmd);
            script.push('\n');
        }
        script.push_str(&assignment.command);

        let spec = RunSpec {
            name: docker::container_name(&assignment.task_id),
            image,
            command: script,
            workspace: root.to_path_buf(),
            env,
            volumes,
            binds,
            workdir,
            timeout_secs: if profile.timeout_secs == 0 {
                rc_core::DEFAULT_TASK_TIMEOUT_SECS
            } else {
                profile.timeout_secs
            },
            memory_mb: self.cfg.memory_mb,
            cpus: self.cfg.cpus,
            pids_limit: self.cfg.pids_limit,
            network: match &self.proxy {
                Some(p) => Network::Egress { proxy: p.clone() },
                None => Network::None,
            },
            labels,
        };
        self.sandbox.run(&spec).await
    }

    /// Read-through cache: the local CAS first, the control plane second.
    async fn blob(&self, hash: &str, client: &mut ServerClient) -> Result<Option<Vec<u8>>> {
        if let Ok(data) = self.cas.get(hash) {
            return Ok(Some(data));
        }
        match client.fetch_blob(hash).await? {
            Some(data) => {
                self.cas.put_trusted(hash, &data)?;
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }

    async fn materialize_bundles(
        &self,
        blobs: &[String],
        client: &mut ServerClient,
    ) -> Result<Vec<PathBuf>> {
        let dir = self.cfg.data_dir.join("bundles");
        std::fs::create_dir_all(&dir)?;
        let mut out = Vec::new();
        for hash in blobs {
            let path = dir.join(format!("{hash}.bundle"));
            if !path.is_file() {
                let Some(data) = self.blob(hash, client).await? else {
                    tracing::warn!(%hash, "bundle blob is gone; continuing without it");
                    continue;
                };
                // Write somewhere unique, then rename: the final path is shared
                // between concurrent tasks, and a half-written bundle at it
                // would be handed to `git bundle list-heads` as if it were
                // complete.
                let tmp = dir.join(format!(".{hash}.{}.part", ulid::Ulid::generate()));
                std::fs::write(&tmp, &data)?;
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.into());
                }
            }
            out.push(path);
        }
        Ok(out)
    }

    pub async fn cancel(&self, task_id: &str) {
        let name = docker::container_name(task_id);
        tracing::info!(task = %task_id, "cancelling");
        let _ = self.sandbox.kill(&name).await;
    }

    /// Build an approved environment image (§8.2).
    pub async fn build_image(&self, order: pb::ImageBuildOrder) -> Option<pb::ImageBuildDone> {
        let env_id = order.env_id.clone();
        if !self.building.lock().await.insert(env_id.clone()) {
            tracing::info!(env = %env_id, "already building this environment; ignoring the repeat order");
            return None;
        }
        let done = self.build_image_inner(order).await;
        self.building.lock().await.remove(&env_id);
        Some(done)
    }

    async fn build_image_inner(&self, order: pb::ImageBuildOrder) -> pb::ImageBuildDone {
        let env_id = order.env_id.clone();
        if !order.dockerfile.trim().is_empty() {
            let tag = if order.image_ref.is_empty() {
                format!("rc-env/{env_id}:latest")
            } else {
                order.image_ref.clone()
            };
            return match self.sandbox.build_image(&tag, &order.dockerfile).await {
                Ok((digest, log)) => pb::ImageBuildDone {
                    env_id,
                    ok: true,
                    digest,
                    message: format!("built {tag}"),
                    log_blob: log.chars().take(64 * 1024).collect(),
                },
                Err(e) => pb::ImageBuildDone {
                    env_id,
                    ok: false,
                    message: e.to_string(),
                    ..Default::default()
                },
            };
        }
        // Upstream image: pulling it is what pins its digest.
        match self.sandbox.ensure_image(&order.pull_ref).await {
            Ok(_) => match self.sandbox.digest_of(&order.pull_ref).await {
                Ok(digest) => pb::ImageBuildDone {
                    env_id,
                    ok: true,
                    digest,
                    message: format!("pulled {}", order.pull_ref),
                    ..Default::default()
                },
                Err(e) => pb::ImageBuildDone {
                    env_id,
                    ok: false,
                    message: e.to_string(),
                    ..Default::default()
                },
            },
            Err(e) => pb::ImageBuildDone {
                env_id,
                ok: false,
                message: e.to_string(),
                ..Default::default()
            },
        }
    }

    /// Caches for worktrees nobody has touched in a while (§9).
    pub async fn gc(&self, idle_days: i64) -> Result<u64> {
        let cutoff = rc_core::now_secs() - idle_days * 24 * 3600;
        let mut reclaimed = 0u64;

        for (name, labels) in self.sandbox.our_volumes().await? {
            let Some(worktree) = labels.get(docker::LABEL_WORKTREE) else {
                continue; // per-project registry caches are kept
            };
            let dir = docker::workspace_dir(&self.cfg.work_dir(), worktree);
            let last_used = std::fs::metadata(&dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if last_used > cutoff {
                continue;
            }
            tracing::info!(volume = %name, %worktree, "reclaiming idle worktree cache");
            if self.sandbox.remove_volume(&name).await.is_ok() {
                reclaimed += 1;
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
        Ok(reclaimed)
    }
}

/// Absolute working directory for a build, given the profile's sub-project
/// path. Empty means the workspace root.
fn subproject_workdir(path: &str) -> Result<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(docker::WORKSPACE_MOUNT.to_string());
    }
    if !rc_core::manifest::is_safe_relative_path(trimmed) {
        return Err(anyhow!(
            "profile path `{path}` is not a safe relative path; refusing to run outside the workspace"
        ));
    }
    Ok(format!("{}/{trimmed}", docker::WORKSPACE_MOUNT))
}

/// Whether to wire the shared compilation cache into builds (§7.2).
///
/// Off unless `RC_ENABLE_SCCACHE=1`, because §7.2 as designed does not work:
/// the sccache *server* runs on the worker host, but it is the server that
/// invokes the compiler, and the path it is handed
/// (`/usr/local/rustup/toolchains/.../bin/rustc`) exists only inside the build
/// container. Every compile then dies on a dropped connection.
///
/// Keying this off "is the binary on PATH", as it used to, means an unrelated
/// `cargo install sccache` silently breaks every build on the machine. The
/// plumbing below is left intact for whoever fixes the design — most likely by
/// running sccache inside the container with `SCCACHE_DIR` on a mounted volume,
/// which trades the credential isolation §7.2 was protecting for a cache that
/// untrusted build code can write to.
fn sccache_enabled() -> bool {
    if std::env::var("RC_ENABLE_SCCACHE").as_deref() != Ok("1") {
        tracing::info!(
            "shared sccache is off (set RC_ENABLE_SCCACHE=1 to force it on); \
             per-worktree target volumes still cache local crates"
        );
        return false;
    }
    match which("sccache") {
        Some(_) => {
            tracing::warn!(
                "RC_ENABLE_SCCACHE=1: mounting the host sccache socket, which only works if \
                 this host has the container's toolchain at the same path"
            );
            true
        }
        None => {
            tracing::warn!("RC_ENABLE_SCCACHE=1 but sccache is not on PATH; leaving it off");
            false
        }
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(program);
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn cfg() -> WorkerConfig {
        let mut c = WorkerConfig::default();
        c.data_dir = std::env::temp_dir().join(format!("rc-runner-{}", ulid::Ulid::generate()));
        c.ensure_dirs().unwrap();
        c
    }

    #[test]
    fn which_finds_a_real_binary_and_rejects_a_fake_one() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn workspace_paths_are_scoped_per_worktree() {
        let c = cfg();
        let a = docker::workspace_dir(&c.work_dir(), "w-a");
        let b = docker::workspace_dir(&c.work_dir(), "w-b");
        assert_ne!(a, b);
        assert!(a.starts_with(c.work_dir()));
    }

    /// The build script the sandbox runs: pre_commands then the task command,
    /// under `set -e` so a failed codegen step does not silently produce a
    /// "compile error" about missing generated files.
    fn build_script(profile: &pb::ResolvedProfile, command: &str) -> String {
        let mut script = String::from("set -e\n");
        for cmd in &profile.pre_commands {
            script.push_str(cmd);
            script.push('\n');
        }
        script.push_str(command);
        script
    }

    #[test]
    fn pre_commands_run_before_the_task_and_abort_on_failure() {
        let profile = pb::ResolvedProfile {
            pre_commands: vec!["cargo run -p xtask codegen".into()],
            ..Default::default()
        };
        let script = build_script(&profile, "cargo check");
        assert!(script.starts_with("set -e\n"));
        assert!(
            script.find("xtask codegen").unwrap() < script.find("cargo check").unwrap(),
            "codegen must precede the build"
        );
    }

    #[test]
    fn a_subproject_path_becomes_the_working_directory() {
        assert_eq!(subproject_workdir("crates/backend").unwrap(), "/work/crates/backend");
        assert_eq!(subproject_workdir("/crates/backend/").unwrap(), "/work/crates/backend");
        assert_eq!(subproject_workdir("").unwrap(), "/work");
    }

    #[test]
    fn the_workspace_mounts_at_the_root_never_at_the_subproject_path() {
        // The bind used to be `workspace:{workdir}`, which aliased the whole
        // repository onto the sub-project's pathname: a build with
        // `path = "crates/backend"` compiled the entire workspace while the
        // real sub-project sat at `/work/crates/backend/crates/backend`.
        let workdir = subproject_workdir("crates/backend").unwrap();
        assert_eq!(docker::WORKSPACE_MOUNT, "/work");
        assert_ne!(workdir, docker::WORKSPACE_MOUNT);
        assert!(
            workdir.starts_with(&format!("{}/", docker::WORKSPACE_MOUNT)),
            "the workdir must live inside the mount, not replace it"
        );
    }

    #[test]
    fn a_traversing_subproject_path_is_refused() {
        for evil in ["../..", "crates/../../etc", "a/./b", "a//b", "..", "C:/win"] {
            assert!(
                subproject_workdir(evil).is_err(),
                "`{evil}` must not become a working directory"
            );
        }
        // A leading slash is a typo, not an escape: it is trimmed and stays
        // inside the mount.
        assert_eq!(subproject_workdir("/etc").unwrap(), "/work/etc");
    }

    #[test]
    fn sccache_is_dropped_when_the_host_daemon_is_absent() {
        // §7.2: RUSTC_WRAPPER pointing at a missing binary fails every compile.
        let profile = pb::ResolvedProfile {
            adapter: "rust".into(),
            ..Default::default()
        };
        let cache = rc_core::adapter::for_name("rust").cache_config(&profile);
        assert!(cache.env.contains_key("RUSTC_WRAPPER"));

        let sccache_available = false;
        let env: Vec<String> = cache
            .env
            .iter()
            .filter(|(k, _)| !(k.as_str() == "RUSTC_WRAPPER" && !(sccache_available && cache.sccache_uds)))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        assert!(!env.iter().any(|e| e.starts_with("RUSTC_WRAPPER=")));
        // The rest of the cache configuration still applies.
        assert!(env.iter().any(|e| e == "CARGO_INCREMENTAL=0"));
    }

    #[tokio::test]
    async fn one_lock_per_worktree_and_it_is_shared() {
        // §6.2: two tasks for the same worktree must serialize; different
        // worktrees must not block each other.
        let locks = KeyedLocks::default();
        let a1 = locks.get("w-a").await;
        let a2 = locks.get("w-a").await;
        let b = locks.get("w-b").await;
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!Arc::ptr_eq(&a1, &b));

        let held = a1.lock().await;
        assert!(a2.try_lock().is_err(), "the second task waits");
        assert!(b.try_lock().is_ok(), "an unrelated worktree proceeds");
        drop(held);
        assert!(a2.try_lock().is_ok());
    }

    #[tokio::test]
    async fn two_worktrees_of_one_project_still_serialize_on_the_mirror() {
        // The worktree lock deliberately lets them run in parallel, but they
        // share `<mirrors>/<project_id>.git`, whose init and fetch sequences
        // are not atomic.
        let worktrees = KeyedLocks::default();
        let projects = KeyedLocks::default();
        assert!(
            !Arc::ptr_eq(&worktrees.get("w-a").await, &worktrees.get("w-b").await),
            "different worktrees run concurrently"
        );

        let p1 = projects.get("p-same").await;
        let p2 = projects.get("p-same").await;
        assert!(Arc::ptr_eq(&p1, &p2), "but they meet at the project mirror");
        let held = p1.lock().await;
        assert!(p2.try_lock().is_err());
        drop(held);
        assert!(p2.try_lock().is_ok());
    }
}
