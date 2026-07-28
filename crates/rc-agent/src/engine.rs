//! The whole `check` pipeline, and the formatting rules that keep an agent's
//! context small (§11, §12).

use crate::client::AgentClient;
use crate::consent;
use crate::excludes;
use crate::multiroot;
use crate::config::AgentConfig;
use crate::index::{KnownBlobs, ResultCache, StatIndex};
use crate::scanner::{self, ScanError};
use anyhow::{anyhow, Context, Result};
use rc_core::model::TaskType;
use rc_core::pb;
use rc_core::profile::{BuildProfile, ProfileSource, Resolution};
use std::path::{Path, PathBuf};

pub struct Engine {
    pub cfg: AgentConfig,
}

#[derive(Debug, Clone)]
pub struct CheckRequest {
    pub path: String,
    pub task: TaskType,
    pub command: Option<String>,
    pub wait_secs: Option<u32>,
    pub no_cache: bool,
}

/// What the agent is told. Deliberately three tiers: a verdict, a short
/// structured summary, and a pointer to paged logs (§11).
#[derive(Debug, Clone)]
pub struct Outcome {
    pub task_id: String,
    pub kind: Option<rc_core::ResultKind>,
    pub status: String,
    pub text: String,
}

impl Engine {
    pub fn new(cfg: AgentConfig) -> Self {
        Engine { cfg }
    }

    async fn client(&self) -> Result<AgentClient> {
        AgentClient::connect(&self.cfg.server, &self.cfg.token).await
    }

    pub async fn check(&self, req: CheckRequest) -> Result<Outcome> {
        let root = resolve_root(&req.path)?;
        let mut client = self.client().await?;

        // ---- identify ----
        let repo_url_probe = scanner::git_root(&root).and(None::<String>);
        let _ = repo_url_probe;
        let mut client_profile = client
            .get_profile(&provisional_project_id(&root), "")
            .await
            .unwrap_or_default();

        // ---- scan (§4.2 torn-snapshot protection lives in here) ----
        let adapter_name = if client_profile.adapter.is_empty() {
            rc_core::adapter::detect(&root)
                .map(|(name, _)| name)
                .unwrap_or_else(|| "generic".into())
        } else {
            client_profile.adapter.clone()
        };
        let adapter = rc_core::adapter::for_name(&adapter_name);
        // The adapter's structural exclusions plus whatever the repository asked
        // to withhold. A bad pattern stops here rather than silently matching
        // nothing — which would look exactly like a working exclusion until the
        // file turned up on the server.
        let excludes = match excludes::Excludes::new(adapter.default_exclude(), &self.repo_excludes(&root)) {
            Ok(e) => e,
            Err(message) => {
                return Ok(Outcome {
                    task_id: String::new(),
                    kind: None,
                    status: "config_error".into(),
                    text: format!(
                        "✗ {} 的 exclude 配置有问题: {message}\n改好再试；控制面不会猜测它的含义。",
                        rc_core::profile::REPO_CONFIG_FILE
                    ),
                })
            }
        };
        // The other direction: files `.gitignore` hides that the build still
        // reads. A bad pattern stops here for the same reason — silently
        // matching nothing looks exactly like a working include until the
        // remote build cannot find the file.
        let includes = match excludes::Includes::new(&self.repo_includes(&root)) {
            Ok(i) => i,
            Err(message) => {
                return Ok(Outcome {
                    task_id: String::new(),
                    kind: None,
                    status: "config_error".into(),
                    text: format!(
                        "✗ {} 的 include 配置有问题: {message}\n改好再试；控制面不会猜测它的含义。",
                        rc_core::profile::REPO_CONFIG_FILE
                    ),
                })
            }
        };

        self.cfg.ensure_dirs()?;

        // ---- roots (§multi-root) ----
        let layout = match self.resolve_roots(&root, adapter.as_ref())? {
            Ok(l) => l,
            Err(message) => {
                return Ok(Outcome {
                    task_id: String::new(),
                    kind: None,
                    status: "needs_approval".into(),
                    text: message,
                })
            }
        };
        if layout.roots.len() > 1 {
            tracing::info!(
                roots = layout.roots.len(),
                anchor = %layout.anchor.display(),
                "the build reaches outside the repository; syncing every root"
            );
        }

        let cfg = &self.cfg;
        let scan = match multiroot::scan_all(&layout, &excludes, &includes, |p| {
            let index = StatIndex::open(&cfg.index_path(p))?;
            if index.is_empty() {
                tracing::info!(root = %p.display(), "cold stat index: this first scan hashes every file");
            }
            Ok(index)
        }) {
            Ok(s) => s,
            Err(ScanError::Unstable { attempts, changed }) => {
                return Ok(Outcome {
                    task_id: String::new(),
                    kind: None,
                    status: "workspace_unstable".into(),
                    text: format!(
                        "⚠ workspace_unstable — 扫描期间有文件在变动（{attempts} 次重试后仍不稳定）。\n\
                         最近变动: {}\n\
                         没有提交编译，因为撕裂的快照会让你去修一个并不存在的错误（§4.2）。稍后重试。",
                        changed.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                    ),
                });
            }
            Err(e) => return Err(anyhow!("{e}")),
        };

        let project_id = rc_core::ids::project_id(scan.repo_url.as_deref(), &root);
        let worktree_id = rc_core::ids::worktree_id(&root, &scan.first_base_commit);
        tracing::debug!(
            hashed = scan.hashed,
            reused = scan.reused,
            is_git = scan.is_git,
            "workspace scanned"
        );

        // The provisional lookup used a path-derived id; redo it now that the
        // remote url is known, so a second worktree of the same repo inherits
        // what the fleet already learned.
        if project_id != provisional_project_id(&root) {
            if let Ok(p) = client.get_profile(&project_id, "").await {
                if p.found || !p.resolved_image.is_empty() {
                    client_profile = p;
                }
            }
        }

        // ---- resolve the build profile (§3.2 priority chain) ----
        let resolution = self.resolve_profile(&root, &req, &client_profile, &adapter_name)?;
        let resolution = match resolution {
            Ok(r) => r,
            Err(message) => {
                return Ok(Outcome {
                    task_id: String::new(),
                    kind: Some(rc_core::ResultKind::EnvError),
                    status: "env_error".into(),
                    text: message,
                })
            }
        };
        let profile_pb = resolution.to_pb();
        let fingerprint =
            rc_core::fingerprint::compute_for(&scan.manifest.root_hash, &profile_pb, &scan.manifest.anchor_mount)
            .map_err(|e| anyhow!("{e}"))?;

        // Withheld files change what the build can see, so this belongs on
        // every answer — including the ones that never reach the network. A
        // remote-only failure caused by an exclusion is otherwise indorsable
        // from an ordinary compile error.
        // Included files are the same story pointed the other way: the repo told
        // git to ignore them and this overrides that, so what got uploaded is
        // named rather than left to be discovered.
        let (egress_hosts, egress_problems) = self.repo_egress(&root);
        let config_note = join_notes(
            join_notes(
                describe_exclusions(&self.repo_excludes(&root)),
                describe_inclusions(includes.patterns()),
            ),
            describe_egress_problems(&egress_problems),
        );

        // ---- local result cache: answer without touching the network ----
        //
        // §7.1: this cache is keyed on what the repository *asked* to reach.
        // What the build's outcome actually depends on is what was *granted*,
        // and only the control plane knows that. There is no way to key a local
        // cache on something this process cannot observe, so a project that
        // declares any egress at all does not get one.
        //
        // Keying on "is an approval pending" was tried and is wrong: a host can
        // be rejected and later approved, or refused by the queue cap, and in
        // both cases nothing is pending while the verdict still hangs on a
        // decision that has not been made yet. Getting that wrong replays a
        // pre-approval failure for a day, which is the exact bug this is here to
        // prevent. The server's cache still answers — it folds the grant into
        // its own key — so the cost is one round trip, not a rebuild.
        let local_cache = local_cache_allowed(&egress_hosts);
        let results = ResultCache::open(&self.cfg.results_path())?;
        if !req.no_cache && local_cache {
            if let Some((task_id, kind, cached)) =
                results.get(&fingerprint, rc_core::TASK_CACHE_TTL_SECS)
            {
                // Rendered now, under this call's limits, and with no synced
                // byte count: this hit moved nothing over the network.
                let rendered = match &cached {
                    Some(result) => {
                        format_result(&task_id, result, self.cfg.max_diagnostics, true, 0)
                    }
                    None => kind.clone(),
                };
                let text = with_note(
                    format!("{rendered}\n(本地指纹缓存命中，未重新编译；task_id={task_id})"),
                    &config_note,
                );
                return Ok(Outcome {
                    task_id: task_id.clone(),
                    kind: Some(rc_core::ResultKind::parse_or_default(&kind)),
                    status: "done".into(),
                    text,
                });
            }
        }

        // A root inside the repository normally needs no permission — it is
        // part of what `check <path>` covers. One that had to be scanned
        // *separately* is different: the repository's own enumeration does not
        // list it, so it is `.gitignore`d, and uploading ignored content is not
        // something the caller asked for. This is the last gate before anything
        // leaves the machine.
        if let Some(message) = self.unapproved_hidden_roots(&root, &scan) {
            return Ok(Outcome {
                task_id: String::new(),
                kind: None,
                status: "needs_approval".into(),
                text: message,
            });
        }

        // ---- sync ----
        let mut known = KnownBlobs::open(&self.cfg.cas_known_path())?;
        let bytes = self.sync_blobs(&mut client, &scan, &mut known).await?;
        let bundle_blob = self
            .sync_baseline(&mut client, &mut known, &project_id, &root, &scan)
            .await
            .unwrap_or_default();

        // ---- submit ----
        let mut submit = pb::SubmitTaskReq {
            project_id: project_id.clone(),
            project_root: root.to_string_lossy().into_owned(),
            repo_url: scan.repo_url.clone().unwrap_or_default(),
            worktree_id: worktree_id.clone(),
            worktree_label: root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            agent_session: self.cfg.agent_session.clone(),
            task_type: req.task.as_str().to_string(),
            command_override: req.command.clone().unwrap_or_default(),
            manifest: Some(scan.manifest.clone()),
            profile: Some(profile_pb),
            fingerprint: fingerprint.clone(),
            bundle_blob,
            no_cache: req.no_cache,
            // A request the control plane records and an administrator decides
            // on. The build is submitted either way: an unapproved host fails
            // the build with a network error, which is a far better answer than
            // refusing to compile anything until someone clicks.
            egress: egress_hosts,
        };

        // What actually got synced beyond the named directory. Approval was
        // given once, in the repo config; this keeps it visible afterwards.
        let root_note = describe_roots(&scan);

        let mut handle = client.submit(submit.clone()).await?;
        if handle.status == "needs_blobs" {
            // The control plane garbage-collected something between our
            // reconcile and this submission (§4.7). Re-upload and try once
            // more; a second miss is a real problem.
            for h in &handle.missing_blobs {
                known.forget(h).ok();
            }
            self.upload_specific(&mut client, &scan, &handle.missing_blobs, &mut known)
                .await?;
            submit.fingerprint = fingerprint.clone();
            handle = client.submit(submit).await?;
            if handle.status == "needs_blobs" {
                return Err(anyhow!(
                    "控制面反复报告缺少 blob（{} 个），同步无法收敛",
                    handle.missing_blobs.len()
                ));
            }
        }

        // Named on the result, not logged and forgotten: a build that cannot
        // reach a host it asked for fails with an ordinary network error, and
        // nothing in that error says an approval is sitting in a queue.
        let config_note = join_notes(
            join_notes(config_note, describe_pending_egress(&handle.egress_pending)),
            describe_refused_egress(&handle.egress_refused),
        );

        if handle.cache_hit {
            let result = handle.result.clone().unwrap_or_default();
            let rendered = format_result(&handle.task_id, &result, self.cfg.max_diagnostics, true, bytes);
            // Only the result is cached. The notes below describe *this*
            // invocation's scan, and are recomputed on every call.
            if local_cache {
                results.put(&fingerprint, &handle.task_id, &result).ok();
            }
            let mut text = with_note(rendered, &root_note);
            text = with_note(text, &config_note);
            text = with_warnings(text, &scan.warnings);
            return Ok(Outcome {
                task_id: handle.task_id,
                kind: Some(rc_core::ResultKind::parse_or_default(&result.kind)),
                status: "done".into(),
                text,
            });
        }

        // ---- short wait, then hand back a handle (§12) ----
        let wait = req.wait_secs.unwrap_or(self.cfg.default_wait_secs);
        let status = client.get_task(&handle.task_id, wait).await?;
        let mut outcome = self.render(&status, bytes);
        outcome.text = with_note(outcome.text, &root_note);
        outcome.text = with_note(outcome.text, &config_note);
        // §4.3: a degraded enumeration is exactly the situation where a
        // remote/local divergence appears, so never hide it.
        outcome.text = with_warnings(outcome.text, &scan.warnings);
        if let Some(result) = &status.result {
            // A build that failed *because* an egress approval had not landed
            // yet must not be cached: the fingerprint does not change when the
            // administrator approves, so the developer would keep being handed
            // the pre-approval failure for the rest of the TTL — from a cache
            // hit that never contacts the server and so never learns the
            // approval happened. The server-side cache is safe from this
            // because it folds the granted set into its own key; the agent
            // cannot, so it declines to remember instead.
            if rc_core::TaskState::parse_or_default(&status.status).is_terminal() {
                if local_cache {
                    results.put(&fingerprint, &status.task_id, result).ok();
                }
                // Fleet learning (§3.2/§1.1 principle 4): the first agent to
                // get a green build teaches every other agent how. A green
                // build is worth teaching whether or not something is still
                // queued for approval — it evidently did not need it.
                if result.kind == "success" && !client_profile.found {
                    self.publish_profile(&mut client, &project_id, &resolution).await;
                }
            }
        }
        Ok(outcome)
    }

    /// Store what worked so the next agent — or the next worktree — inherits
    /// it instead of rediscovering it.
    async fn publish_profile(&self, client: &mut AgentClient, project_id: &str, resolution: &Resolution) {
        let mut profile = BuildProfile {
            adapter: Some(resolution.adapter.clone()),
            image: Some(resolution.image_digest.clone()),
            ..Default::default()
        };
        profile
            .tasks
            .insert(resolution.task_type.as_str().to_string(), resolution.command.clone());
        if !resolution.profile.env.is_empty() {
            profile.env = resolution.profile.env.clone();
        }
        profile.target = resolution.profile.target.clone();
        profile.toolchain = resolution.profile.toolchain.clone();
        // §3.2: a green build that needed a codegen step did not need it by
        // accident, so the next agent should not have to work it out again.
        // Sent as a *request*: the control plane keeps it out of the profile it
        // hands other agents until an administrator has read it, because unlike
        // every other field here this one is a program that will run inside
        // their sandbox.
        if !resolution.profile.pre_commands.clone().unwrap_or_default().is_empty() {
            profile.pre_commands = resolution.profile.pre_commands.clone();
        }

        let req = pb::UpsertProfileReq {
            project_id: project_id.to_string(),
            path: String::new(),
            config_toml: rc_core::profile::to_toml(&profile),
            agent_session: self.cfg.agent_session.clone(),
        };
        // Best effort: failing to share knowledge must never fail the check.
        if let Err(e) = client.upsert_profile(req).await {
            tracing::debug!(error = %e, "could not publish the build profile");
        }
    }

    pub async fn get_result(&self, task_id: &str, wait_secs: u32) -> Result<Outcome> {
        let mut client = self.client().await?;
        let status = client.get_task(task_id, wait_secs).await?;
        Ok(self.render(&status, 0))
    }

    fn render(&self, status: &pb::TaskStatus, bytes: u64) -> Outcome {
        let state = rc_core::TaskState::parse_or_default(&status.status);
        if !state.is_terminal() {
            let phase = status
                .timeline
                .last()
                .map(|p| p.phase.clone())
                .unwrap_or_else(|| status.status.clone());
            return Outcome {
                task_id: status.task_id.clone(),
                kind: None,
                status: status.status.clone(),
                text: format!(
                    "⏳ 仍在执行（当前阶段: {phase}）\ntask_id={}\n用 get_result(task_id) 继续轮询，成本极低。",
                    status.task_id
                ),
            };
        }
        if state == rc_core::TaskState::Superseded {
            return Outcome {
                task_id: status.task_id.clone(),
                kind: None,
                status: status.status.clone(),
                text: format!(
                    "↷ 该任务已被同 session 的新代码取代（superseded_by={}）。用新 task_id 查询。",
                    status.superseded_by
                ),
            };
        }
        let result = status.result.clone().unwrap_or_default();
        let text = format_result(&status.task_id, &result, self.cfg.max_diagnostics, false, bytes);
        Outcome {
            task_id: status.task_id.clone(),
            kind: Some(rc_core::ResultKind::parse_or_default(&result.kind)),
            status: status.status.clone(),
            text,
        }
    }

    /// Work out which local directories this build needs, and whether the user
    /// has agreed to sync the ones outside the repository.
    ///
    /// `Ok(Err(message))` means "stop and show this" — either discovery could
    /// not promise a complete answer, or a root is awaiting approval. Neither
    /// is a failure of the build; both are situations where guessing would be
    /// worse than asking.
    #[allow(clippy::type_complexity)]
    fn resolve_roots(
        &self,
        root: &Path,
        adapter: &dyn rc_core::adapter::Adapter,
    ) -> Result<Result<rc_core::roots::Layout, String>> {
        let discovery = adapter.extra_roots(root);
        if !discovery.complete {
            // Falling back to a narrower root set here would reproduce exactly
            // the fingerprint this project had before it grew the dependency,
            // and the agent would then answer from cache — a stale success with
            // no warning attached (§4.3).
            return Ok(Err(format!(
                "✗ 无法确定构建需要哪些目录，因此没有提交。\n{}\n\n\
                 继续的办法：修好上面的问题，或在 {} 里用 extra_roots 显式列出仓库外的目录。",
                discovery.notes.join("\n"),
                rc_core::profile::REPO_CONFIG_FILE
            )));
        }

        let external = multiroot::external_to(root, &discovery.roots);
        let policy = self.repo_extra_roots(root);
        let approved = match consent::evaluate(root, &external, policy.as_ref()) {
            consent::Consent::Approved(roots) => roots,
            consent::Consent::Blocked { message, .. } => return Ok(Err(message)),
        };

        // Roots inside the repository need no permission — they are already
        // part of what `check <path>` covers — but they still take part in the
        // layout, because one of them may be `.gitignore`d and therefore
        // invisible to the repository's own enumeration.
        let mut candidates = approved;
        candidates.extend(
            discovery
                .roots
                .iter()
                .filter(|p| p.starts_with(root))
                .cloned(),
        );

        match rc_core::roots::compute(root, &candidates) {
            Ok(layout) => Ok(Ok(layout)),
            Err(e) => Ok(Err(format!(
                "✗ 这些目录无法组成一个可用的容器布局：{e}\n\
                 把它们放到同一棵目录树下，或用 extra_roots = [] 关掉仓库外同步。"
            ))),
        }
    }

    /// Roots inside the repository that its own enumeration does not cover, and
    /// which the config has not approved either.
    ///
    /// These are `.gitignore`d directories that cargo nonetheless builds. They
    /// have to be synced for the build to work, but a repository ignores a
    /// directory for a reason, so uploading it is the user's call.
    fn unapproved_hidden_roots(&self, root: &Path, scan: &multiroot::MultiScan) -> Option<String> {
        let hidden: Vec<PathBuf> = scan
            .scanned
            .iter()
            .filter(|r| r.nested && r.path.starts_with(root))
            .map(|r| r.path.clone())
            .collect();
        if hidden.is_empty() {
            return None;
        }
        match consent::evaluate(root, &hidden, self.repo_extra_roots(root).as_ref()) {
            consent::Consent::Approved(_) => None,
            consent::Consent::Blocked { message, .. } => Some(format!(
                "{message}\n\n（这些目录在仓库内，但被 .gitignore 排除，仓库自身的枚举看不到它们；\
                 cargo 却要用它们构建。）"
            )),
        }
    }

    /// `exclude` as declared by the repository itself. Like `extra_roots`, only
    /// the repo file counts: a pattern learned from the fleet deciding what does
    /// or does not leave this machine would be exactly backwards.
    fn repo_excludes(&self, root: &Path) -> Vec<String> {
        std::fs::read_to_string(root.join(rc_core::profile::REPO_CONFIG_FILE))
            .ok()
            .and_then(|text| rc_core::profile::parse_toml(&text).ok())
            .and_then(|p| p.profile.exclude)
            .unwrap_or_default()
    }

    /// `egress` as declared by the repository itself. Only the repo file counts,
    /// for the same reason as `exclude` and `include`: this widens what the
    /// sandbox can reach, and a fleet-learned value doing that for a project
    /// that never asked would be exactly backwards.
    /// Returns the hosts worth sending and the problems worth telling the agent
    /// about. Validating here is what turns a typo into a config error instead
    /// of a build that fails much later with an ordinary network message; the
    /// control plane validates again, because it trusts no agent.
    ///
    /// A malformed entry withholds only itself. `normalize_all` is
    /// all-or-nothing by design — a config with three typos should take one
    /// round trip to fix — but applying that here would mean one bad line
    /// silently dropping every good one, and the whole point is that nothing
    /// about egress should be silent.
    fn repo_egress(&self, root: &Path) -> (Vec<String>, Vec<String>) {
        let declared = std::fs::read_to_string(root.join(rc_core::profile::REPO_CONFIG_FILE))
            .ok()
            .and_then(|text| rc_core::profile::parse_toml(&text).ok())
            .and_then(|p| p.profile.egress)
            .unwrap_or_default();
        let mut hosts = Vec::new();
        let mut problems = Vec::new();
        for entry in &declared {
            match rc_core::egress::normalize(entry) {
                Ok(host) => hosts.push(host),
                Err(e) => problems.push(e),
            }
        }
        hosts.sort();
        hosts.dedup();
        (hosts, problems)
    }

    /// `include` as declared by the repository itself. Only the repo file
    /// counts, for the same reason `exclude` is read this way: it decides what
    /// leaves this machine, and a fleet-learned pattern deciding that for a
    /// project it has never seen would be exactly backwards.
    fn repo_includes(&self, root: &Path) -> Vec<String> {
        std::fs::read_to_string(root.join(rc_core::profile::REPO_CONFIG_FILE))
            .ok()
            .and_then(|text| rc_core::profile::parse_toml(&text).ok())
            .and_then(|p| p.profile.include)
            .unwrap_or_default()
    }

    /// `extra_roots` as declared by the repository itself. Only the repo file
    /// counts: this is a decision about the user's own data, so a value learned
    /// from the fleet or guessed by an adapter has no business granting it.
    fn repo_extra_roots(&self, root: &Path) -> Option<rc_core::profile::ExtraRoots> {
        let text = std::fs::read_to_string(root.join(rc_core::profile::REPO_CONFIG_FILE)).ok()?;
        rc_core::profile::parse_toml(&text).ok()?.profile.extra_roots
    }

    /// §3.2 chain: explicit > repo file > server > detected.
    #[allow(clippy::type_complexity)]
    fn resolve_profile(
        &self,
        root: &Path,
        req: &CheckRequest,
        server: &pb::ProfileResp,
        adapter_name: &str,
    ) -> Result<Result<Resolution, String>> {
        let mut layers: Vec<(ProfileSource, BuildProfile)> = Vec::new();

        // Explicit call arguments.
        let mut explicit = BuildProfile::default();
        if let Some(cmd) = &req.command {
            explicit.tasks.insert(req.task.as_str().to_string(), cmd.clone());
        }
        layers.push((ProfileSource::Explicit, explicit));

        // Repo config: versioned, reviewable, travels with the branch (§3.2).
        let repo_path = root.join(rc_core::profile::REPO_CONFIG_FILE);
        if let Ok(text) = std::fs::read_to_string(&repo_path) {
            match rc_core::profile::parse_toml(&text) {
                Ok(parsed) => layers.push((ProfileSource::Repo, parsed.profile)),
                Err(e) => {
                    return Ok(Err(format!(
                        "✗ {} 解析失败: {e}\n修好它再试；控制面不会猜测其内容。",
                        rc_core::profile::REPO_CONFIG_FILE
                    )))
                }
            }
        }

        // What the fleet already knows.
        if !server.config_toml.is_empty() {
            if let Ok(parsed) = rc_core::profile::parse_toml(&server.config_toml) {
                layers.push((ProfileSource::Server, parsed.profile));
            }
        }

        // Adapter auto-detection.
        if let Some((_, detected)) = rc_core::adapter::detect(root) {
            layers.push((ProfileSource::Detected, detected));
        }

        let (mut merged, source) = rc_core::profile::resolve(layers);

        // Host env the adapter declares as build-affecting is absorbed here so
        // it lands in the fingerprint (§10.1/§5.1).
        let adapter = rc_core::adapter::for_name(adapter_name);
        for key in adapter.relevant_env() {
            if let Ok(value) = std::env::var(key) {
                merged.env.entry((*key).to_string()).or_insert(value);
            }
        }

        // The image must be a digest before anything is hashed (§5.1).
        let image = merged
            .image
            .clone()
            .filter(|i| rc_core::fingerprint::is_digest_ref(i))
            .or_else(|| {
                Some(server.resolved_image.clone()).filter(|i| rc_core::fingerprint::is_digest_ref(i))
            });
        let Some(image_digest) = image else {
            let hints = rc_core::adapter::system_dep_hints(root);
            let hint_text = if hints.is_empty() {
                String::new()
            } else {
                format!("\n检测到可能需要的系统依赖: {}", hints.join(", "))
            };
            return Ok(Err(format!(
                "✗ env_error — 还没有可用的已审批编译环境。{}{hint_text}\n\
                 下一步: list_envs(query=\"{adapter_name}\") 查找现成镜像；没有就 prepare_env(dockerfile=...) 提交一个（异步，不阻塞你继续写代码）。",
                if server.message.is_empty() { String::new() } else { format!("\n{}", server.message) }
            )));
        };

        let command = match &req.command {
            Some(c) => c.clone(),
            None => match merged.tasks.get(req.task.as_str()) {
                Some(c) => c.clone(),
                None => adapter.command_for(&merged, req.task).line,
            },
        };
        let toolchain = merged.toolchain.clone().unwrap_or_default();

        Ok(Ok(Resolution {
            profile: merged,
            source,
            task_type: req.task,
            command,
            adapter: adapter_name.to_string(),
            image_digest,
            toolchain,
        }))
    }

    /// Reconcile and upload the dirty layer (§4.1 L2).
    async fn sync_blobs(
        &self,
        client: &mut AgentClient,
        scan: &multiroot::MultiScan,
        known: &mut KnownBlobs,
    ) -> Result<u64> {
        let needed = rc_core::manifest::blobs_to_reconcile(&scan.manifest);
        // The local hint is only a hint: expired entries go back on the wire
        // (§4.7).
        let (_already, ask) = known.partition(&needed);
        let missing = client.check_blobs(ask.clone(), &self.cfg.agent_session).await?;
        let confirmed: Vec<String> = ask
            .into_iter()
            .filter(|h| !missing.contains(h))
            .collect();
        known.note(&confirmed).ok();

        if missing.is_empty() {
            return Ok(0);
        }
        let bytes = self.upload_specific(client, scan, &missing, known).await?;
        Ok(bytes)
    }

    async fn upload_specific(
        &self,
        client: &mut AgentClient,
        scan: &multiroot::MultiScan,
        hashes: &[String],
        known: &mut KnownBlobs,
    ) -> Result<u64> {
        use std::collections::HashMap;
        let wanted: std::collections::HashSet<&String> = hashes.iter().collect();
        let mut by_hash: HashMap<String, PathBuf> = HashMap::new();
        for entry in &scan.manifest.entries {
            if wanted.contains(&entry.hash) && !by_hash.contains_key(&entry.hash) {
                // Manifest paths are relative to the anchor, which is the
                // primary root only when there is nothing else to sync.
                by_hash.insert(entry.hash.clone(), scan.anchor.join(&entry.path));
            }
        }

        let mut batch = Vec::new();
        let mut batch_bytes = 0u64;
        let mut total = 0u64;
        let mut uploaded = Vec::new();
        for (hash, path) in by_hash {
            let data = std::fs::read(&path)
                .with_context(|| format!("read {} for upload", path.display()))?;
            // The file may have changed since the scan; uploading the old hash
            // with new bytes would poison the CAS.
            if rc_core::cas::hash_bytes(&data) != hash {
                return Err(anyhow!(
                    "workspace_unstable: {} changed while uploading; retry",
                    path.display()
                ));
            }
            total += data.len() as u64;
            batch_bytes += data.len() as u64;
            uploaded.push(hash.clone());
            batch.push((hash, data));
            // Keep a bounded amount in memory: a worktree can hold gigabytes.
            if batch_bytes > 64 * 1024 * 1024 {
                client.upload_blobs(std::mem::take(&mut batch)).await?;
                batch_bytes = 0;
            }
        }
        if !batch.is_empty() {
            client.upload_blobs(batch).await?;
        }
        known.note(&uploaded).ok();
        Ok(total)
    }

    /// Make the baseline commit reachable for the fleet (§4.1 steps 1–3).
    async fn sync_baseline(
        &self,
        client: &mut AgentClient,
        known: &mut KnownBlobs,
        project_id: &str,
        root: &Path,
        scan: &multiroot::MultiScan,
    ) -> Result<String> {
        if !scan.manifest.baseline || scan.manifest.base_commit.is_empty() {
            return Ok(String::new());
        }
        let baseline = client
            .get_baseline(project_id, &scan.manifest.base_commit)
            .await?;
        if baseline.have {
            return Ok(String::new());
        }
        // The commit is very likely unpushed — agents commit locally all the
        // time — so ship it as a bundle rather than hoping a fetch works.
        let data = match scanner::create_bundle(root, &scan.manifest.base_commit, &baseline.known_commits) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "bundle creation failed; degrading to full L2 sync");
                return Ok(String::new());
            }
        };
        let hash = rc_core::cas::hash_bytes(&data);
        client.upload_blobs(vec![(hash.clone(), data)]).await?;
        known.note(std::slice::from_ref(&hash)).ok();
        client
            .register_bundle(pb::BundleUpload {
                project_id: project_id.to_string(),
                base_commit: scan.manifest.base_commit.clone(),
                blob_hash: hash.clone(),
                base_commits: baseline.known_commits,
            })
            .await?;
        Ok(hash)
    }

    pub async fn get_log(&self, query: pb::LogQuery) -> Result<pb::LogChunk> {
        let mut client = self.client().await?;
        client.get_log(query).await
    }

    pub async fn build_profile(&self, path: &str) -> Result<pb::ProfileResp> {
        let root = resolve_root(path)?;
        let mut client = self.client().await?;
        let repo_url = scanner::git_root(&root)
            .and_then(|_| std::process::Command::new("git")
                .current_dir(&root)
                .args(["config", "--get", "remote.origin.url"])
                .output()
                .ok())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let project_id = rc_core::ids::project_id(repo_url.as_deref(), &root);
        client.get_profile(&project_id, "").await
    }

    pub async fn list_envs(&self, req: pb::ListEnvsReq) -> Result<Vec<pb::EnvImage>> {
        self.client().await?.list_envs(req).await
    }

    pub async fn prepare_env(&self, mut req: pb::PrepareEnvReq) -> Result<pb::EnvStatus> {
        req.agent_session = self.cfg.agent_session.clone();
        self.client().await?.prepare_env(req).await
    }

    pub async fn env_status(&self, env_id: &str) -> Result<pb::EnvStatus> {
        self.client().await?.get_env_status(env_id).await
    }

    pub async fn list_workers(&self) -> Result<pb::ListWorkersResp> {
        self.client().await?.list_workers().await
    }
}

/// One line naming every directory beyond the primary root that took part in
/// this build. Silent when there is only one, which is the common case — the
/// point is that syncing a sibling checkout never becomes invisible routine.
fn describe_roots(scan: &multiroot::MultiScan) -> String {
    if scan.scanned.len() < 2 {
        return String::new();
    }
    let others: Vec<String> = scan
        .scanned
        .iter()
        .filter(|r| !r.primary)
        .map(|r| r.mount.clone())
        .collect();
    format!("同步了 {} 个目录，除主仓库外还有: {}", scan.scanned.len(), others.join(", "))
}

/// Named on every result, not just the first: an excluded file the build turns
/// out to need fails remotely and passes locally, and the diagnostics for that
/// look like an ordinary missing-file error with no hint of the cause.
fn describe_exclusions(patterns: &[String]) -> String {
    if patterns.is_empty() {
        return String::new();
    }
    format!(
        "已按 exclude 排除 ({})：这些文件没有同步，若构建需要它们，远程会失败而本地不会。",
        patterns.join(", ")
    )
}

/// The mirror of `describe_exclusions`: `include` uploads files the repository
/// told git to ignore, which is a disclosure the caller should see named rather
/// than infer from a byte count.
fn describe_inclusions(patterns: &[String]) -> String {
    if patterns.is_empty() {
        return String::new();
    }
    format!(
        "已按 include 附加同步 ({})：这些文件被 .gitignore 忽略，但仓库声明构建需要它们。",
        patterns.join(", ")
    )
}

/// Egress the repository asked for and nobody has approved yet (§7.1).
fn describe_pending_egress(hosts: &[String]) -> String {
    if hosts.is_empty() {
        return String::new();
    }
    format!(
        "⏳ egress 待审批 ({})：这些域名在构建里仍然不可达，需要管理员批准后才生效。",
        hosts.join(", ")
    )
}

/// Egress entries that were never sent because they are not host names. Said
/// here, at submit time, rather than left to surface as a network failure in a
/// build log — the control plane would only log a warning nobody reads.
fn describe_egress_problems(problems: &[String]) -> String {
    if problems.is_empty() {
        return String::new();
    }
    format!(
        "⚠ egress 配置有误，以下条目已被忽略：\n  {}",
        problems.join("\n  ")
    )
}

/// Whether this project's verdicts may be remembered on this machine (§7.1).
///
/// A named function rather than an inline `is_empty()` because the reasoning is
/// not obvious from the expression: the local cache is keyed on a fingerprint
/// that contains what the repository *asked* to reach, while the outcome
/// depends on what an administrator *granted* — state this process cannot see.
/// Anything that can be settled later by a decision made elsewhere must not be
/// remembered here, or an approval arrives and the developer keeps being handed
/// the failure that preceded it.
fn local_cache_allowed(egress_hosts: &[String]) -> bool {
    egress_hosts.is_empty()
}

/// Hosts the control plane would not even queue for approval, because this
/// project is at its cap. They are not pending, so nothing else will ever
/// mention them — and the build will fail with a network error that looks
/// exactly like a host nobody has got round to approving yet.
fn describe_refused_egress(hosts: &[String]) -> String {
    if hosts.is_empty() {
        return String::new();
    }
    format!(
        "✗ egress 未受理 ({})：该 project 的审批队列已满，这些域名没有被登记，\
         需要管理员先清理已有条目。",
        hosts.join(", ")
    )
}

fn join_notes(a: String, b: String) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b,
        (_, true) => a,
        _ => format!("{a}\n{b}"),
    }
}

fn with_note(text: String, note: &str) -> String {
    if note.is_empty() {
        return text;
    }
    format!("{text}\n{note}")
}

fn with_warnings(text: String, warnings: &[String]) -> String {
    if warnings.is_empty() {
        return text;
    }
    format!("{text}\n\n⚠ {}", warnings.join("\n⚠ "))
}

fn provisional_project_id(root: &Path) -> String {
    rc_core::ids::project_id(None, root)
}

pub fn resolve_root(path: &str) -> Result<PathBuf> {
    let raw = PathBuf::from(shellexpand_home(path));
    let canonical = raw
        .canonicalize()
        .with_context(|| format!("path does not exist: {path}"))?;
    if canonical.is_file() {
        return Ok(canonical
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(canonical));
    }
    // Prefer the repository root: a check run from a subdirectory should still
    // build the whole workspace the developer sees.
    Ok(scanner::git_root(&canonical).unwrap_or(canonical))
}

fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    }
    path.to_string()
}

/// L0 verdict + L1 structured summary + a pointer to L2 (§11). Everything an
/// agent does not need is left out: no ANSI, no rustc notes, no full log.
pub fn format_result(
    task_id: &str,
    result: &pb::TaskResult,
    max_diagnostics: usize,
    cache_hit: bool,
    bytes_synced: u64,
) -> String {
    let kind = rc_core::ResultKind::parse_or_default(&result.kind);
    let mut out = String::new();

    let headline = match kind {
        rc_core::ResultKind::Success => format!("✓ {}", result.summary),
        _ => format!("✗ {} [{}]", result.summary, kind.as_str()),
    };
    out.push_str(&headline);
    out.push('\n');
    out.push_str(&format!("task_id={task_id}"));
    if cache_hit {
        out.push_str("  (cache hit)");
    }
    if bytes_synced > 0 {
        out.push_str(&format!("  synced={}", human_bytes(bytes_synced)));
    }
    if let Some(stats) = &result.stats {
        if stats.build_ms > 0 {
            out.push_str(&format!("  build={}ms", stats.build_ms));
        }
    }
    out.push('\n');

    if kind != rc_core::ResultKind::Success {
        out.push_str(kind.agent_hint());
        out.push('\n');
    }
    // Named before the diagnostics, because for an env_error there are none —
    // and this is the only thing in the result an agent can act on without
    // paging the log.
    for line in &result.env_hints {
        out.push_str(line);
        out.push('\n');
    }

    let shown: Vec<&pb::Diagnostic> = result.diagnostics.iter().take(max_diagnostics).collect();
    if !shown.is_empty() {
        out.push('\n');
        for d in &shown {
            let location = if d.file.is_empty() {
                String::new()
            } else if d.line > 0 {
                format!("{}:{}:{}  ", d.file, d.line, d.column)
            } else {
                format!("{}  ", d.file)
            };
            let code = if d.code.is_empty() {
                String::new()
            } else {
                format!("{}  ", d.code)
            };
            out.push_str(&format!("{}{}{}{}\n", level_mark(&d.level), location, code, d.message));
        }
    }

    let hidden = result
        .diagnostics
        .len()
        .saturating_sub(shown.len()) as u32
        + result.truncated_diagnostics;
    if hidden > 0 {
        out.push_str(&format!("\n… 另有 {hidden} 条诊断未展示。"));
    }
    if kind != rc_core::ResultKind::Success {
        out.push_str(&format!(
            "\n需要细节: get_log(task_id=\"{task_id}\", grep=\"error\", limit=50)"
        ));
    }
    out
}

fn level_mark(level: &str) -> &'static str {
    match level {
        "error" => "E ",
        "warning" => "W ",
        _ => "  ",
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn diag(level: &str, file: &str, line: u32, code: &str, msg: &str) -> pb::Diagnostic {
        pb::Diagnostic {
            level: level.into(),
            code: code.into(),
            message: msg.into(),
            file: file.into(),
            line,
            column: 5,
            rendered: "rendered text that must not be dumped inline".into(),
        }
    }

    #[test]
    fn success_is_one_short_line_plus_the_handle() {
        let result = pb::TaskResult {
            kind: "success".into(),
            summary: "success".into(),
            ..Default::default()
        };
        let text = format_result("t-1", &result, 10, false, 0);
        assert!(text.starts_with("✓ success"));
        assert!(text.contains("task_id=t-1"));
        // §11: no log pointer and no advice when there is nothing to fix.
        assert!(!text.contains("get_log"));
        assert!(text.lines().count() <= 3, "success must stay tiny:\n{text}");
    }

    #[test]
    fn compile_errors_show_locations_but_never_the_rendered_block() {
        let result = pb::TaskResult {
            kind: "compile_error".into(),
            summary: "2 errors, 1 warnings".into(),
            diagnostics: vec![
                diag("error", "src/main.rs", 7, "E0308", "mismatched types"),
                diag("error", "src/lib.rs", 22, "E0433", "failed to resolve"),
            ],
            error_count: 2,
            warning_count: 1,
            ..Default::default()
        };
        let text = format_result("t-1", &result, 10, false, 0);
        assert!(text.contains("src/main.rs:7:5  E0308  mismatched types"));
        assert!(text.contains("修改源码"), "the next action must be spelled out:\n{text}");
        assert!(text.contains("get_log"));
        assert!(
            !text.contains("rendered text"),
            "the rendered block is pure token cost inline (§11)"
        );
    }

    #[test]
    fn an_env_error_names_the_missing_dependency_inline() {
        // An env_error carries no diagnostics, so without this the agent's only
        // route to the cause is paging a multi-thousand-line log — the exact
        // context spend the whole system exists to avoid (§11).
        let result = pb::TaskResult {
            kind: "env_error".into(),
            summary: "环境错误（exit 101）：error: failed to run custom build command".into(),
            env_hints: rc_core::envdep::hint_lines(&rc_core::envdep::analyze("Could not find librrd")),
            exit_code: 101,
            ..Default::default()
        };
        let text = format_result("t-1", &result, 10, false, 0);
        assert!(text.contains("librrd"), "{text}");
        assert!(text.contains("librrd-dev"), "{text}");
        assert!(text.contains("prepare_env"), "{text}");
    }

    #[test]
    fn diagnostics_are_capped_and_the_remainder_is_counted() {
        let result = pb::TaskResult {
            kind: "compile_error".into(),
            summary: "40 errors".into(),
            diagnostics: (0..40)
                .map(|i| diag("error", "a.rs", i, "E0001", "boom"))
                .collect(),
            truncated_diagnostics: 5,
            ..Default::default()
        };
        let text = format_result("t-1", &result, 10, false, 0);
        assert_eq!(text.matches("E0001").count(), 10);
        assert!(text.contains("另有 35 条诊断未展示"));
    }

    #[test]
    fn each_result_kind_carries_its_own_next_step() {
        // §3.5: the agent's next action differs completely per kind.
        for (kind, needle) in [
            ("env_error", "prepare_env"),
            ("infra_error", "无需修改代码"),
            ("timeout", "timeout_secs"),
        ] {
            let result = pb::TaskResult {
                kind: kind.into(),
                summary: "boom".into(),
                ..Default::default()
            };
            let text = format_result("t-1", &result, 10, false, 0);
            assert!(text.contains(needle), "{kind} should mention {needle}:\n{text}");
            assert!(text.contains(&format!("[{kind}]")));
        }
    }

    #[test]
    fn a_cache_hit_is_labelled_so_the_agent_knows_nothing_recompiled() {
        let result = pb::TaskResult {
            kind: "success".into(),
            summary: "success".into(),
            ..Default::default()
        };
        assert!(format_result("t-1", &result, 10, true, 0).contains("cache hit"));
    }

    #[test]
    fn sync_volume_is_reported_only_when_something_moved() {
        let result = pb::TaskResult {
            kind: "success".into(),
            summary: "success".into(),
            ..Default::default()
        };
        assert!(format_result("t", &result, 10, false, 2048).contains("synced=2.0KB"));
        assert!(!format_result("t", &result, 10, false, 0).contains("synced"));
    }

    #[test]
    fn a_project_that_declares_egress_is_never_remembered_locally() {
        assert!(local_cache_allowed(&[]), "an ordinary project keeps its local cache");
        assert!(
            !local_cache_allowed(&["registry.corp".to_string()]),
            "a verdict that hangs on an approval must not be cached where the \
             approval cannot be seen"
        );
    }

    #[test]
    fn a_bad_egress_line_withholds_only_itself_and_is_named() {
        let root = std::env::temp_dir().join(format!("rc-egress-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(rc_core::profile::REPO_CONFIG_FILE),
            "egress = [\"ok.example.com\", \"127.0.0.1\", \"https://x/y\", \"OK.example.com\"]\n",
        )
        .unwrap();

        let engine = Engine::new(AgentConfig::default());
        let (hosts, problems) = engine.repo_egress(&root);
        // The good line survives, lowercased and deduplicated…
        assert_eq!(hosts, vec!["ok.example.com"]);
        // …and both bad ones are reported rather than silently dropped.
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("IP address")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("host names")), "{problems:?}");

        let note = describe_egress_problems(&problems);
        assert!(note.contains("127.0.0.1"), "{note}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn byte_formatting_stays_readable() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0MB");
    }

    #[test]
    fn a_path_inside_a_repo_resolves_to_the_repo_root() {
        let root = std::env::temp_dir().join(format!("rc-root-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(root.join("crates/inner/src")).unwrap();
        std::process::Command::new("git")
            .current_dir(&root)
            .args(["init", "--quiet"])
            .output()
            .unwrap();
        let resolved = resolve_root(&root.join("crates/inner/src").to_string_lossy()).unwrap();
        assert_eq!(resolved.canonicalize().unwrap(), root.canonicalize().unwrap());
    }

    #[test]
    fn a_file_path_resolves_to_its_directory() {
        let dir = std::env::temp_dir().join(format!("rc-file-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "x").unwrap();
        let resolved = resolve_root(&dir.join("main.rs").to_string_lossy()).unwrap();
        assert_eq!(resolved.canonicalize().unwrap(), dir.canonicalize().unwrap());
    }

    #[test]
    fn a_missing_path_says_so_plainly() {
        let err = resolve_root("/definitely/not/here").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }
}
