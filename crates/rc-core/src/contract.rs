//! Task semantic contracts (mechanism two).
//!
//! Each task type is a five-tuple: default command, default env, outcome
//! parser, outcome renderer, and rule-keyed remediation. The env layer feeds
//! the single resolve_env/canonicalize path so fingerprint and execution
//! always see the same effective profile.

use crate::diag;
use crate::model::TaskType;
use crate::pb::TestSummary;
use crate::profile::BuildProfile;
use std::collections::BTreeMap;

/// What a completed task concluded, independent of attribution.
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    Success { summary: String },
    Test {
        summary: TestSummary,
        /// Whether the command was the contract default (parser was enabled).
        parsed: bool,
    },
    Custom { exit_code: i32 },
}

#[derive(Debug, Clone)]
pub struct ConclusionParts {
    pub headline: String,
    pub details: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Remediation {
    pub env_patch: Vec<(String, String)>,
    pub note: String,
}

/// Flags that shape the default command (features/target already live on the
/// profile; this is a thin hook for future options).
#[derive(Debug, Default, Clone)]
pub struct TaskFlags {
    pub profile: BuildProfile,
    /// Intent path scope; empty = workspace default.
    pub path: crate::pb::PathContext,
}

pub trait TaskContract: Send + Sync {
    fn task_type(&self) -> TaskType;
    fn default_command(&self, flags: &TaskFlags) -> String;
    fn default_env(&self) -> &[(&'static str, &'static str)];
    fn parse_outcome(&self, exit_code: i32, log: &str, command_is_default: bool) -> TaskOutcome;
    fn render_outcome(&self, o: &TaskOutcome) -> ConclusionParts;
    fn remediation(&self, rule: &str) -> Option<Remediation>;
}

pub fn for_task(task: TaskType) -> Box<dyn TaskContract> {
    match task {
        TaskType::Check => Box::new(CheckContract),
        TaskType::Build => Box::new(BuildContract),
        TaskType::Test => Box::new(TestContract),
        TaskType::Clippy => Box::new(CheckContract), // same shape as check
        TaskType::Custom => Box::new(CustomContract),
    }
}

// ---- shared remediation for resource rules --------------------------------

fn resource_remediation(rule: &str) -> Option<Remediation> {
    match rule {
        // TestContract default_env already sets debuginfo=0; OOM remediation
        // lowers compile concurrency instead (second-round revision of §2.5).
        "oom_killed" | "sigkill_suspected_oom" => Some(Remediation {
            env_patch: vec![("CARGO_BUILD_JOBS".into(), "2".into())],
            note: "首次尝试疑似 OOM 失败，已自动以 CARGO_BUILD_JOBS=2 降并发重试".into(),
        }),
        _ => None,
    }
}

/// Whether libtest parsing may run: only for `task=test` whose effective
/// command is the contract/adapter default (not request override, not profile
/// `tasks.test` override). Shared by agent and worker (R1).
pub fn command_is_default(
    task_type: TaskType,
    command_override: &str,
    profile_tasks: &std::collections::HashMap<String, String>,
) -> bool {
    if task_type != TaskType::Test {
        return false;
    }
    if !command_override.is_empty() {
        return false;
    }
    // A profile that names a custom test command (e.g. nextest) disables the parser.
    !profile_tasks.contains_key(task_type.as_str())
}

/// Worker-side gate: only the resolved command and profile are available.
/// Returns true iff the command is what the TestContract would emit for this
/// profile's target/features and no profile task override is present.
pub fn command_is_default_resolved(
    task_type: TaskType,
    command: &str,
    profile: &crate::pb::ResolvedProfile,
) -> bool {
    if task_type != TaskType::Test {
        return false;
    }
    if profile.tasks.contains_key(task_type.as_str()) {
        return false;
    }
    let bp = crate::profile::BuildProfile {
        adapter: Some(profile.adapter.clone()),
        target: if profile.target.is_empty() {
            None
        } else {
            Some(profile.target.clone())
        },
        features: if profile.features.is_empty() {
            None
        } else {
            Some(profile.features.clone())
        },
        ..Default::default()
    };
    let default = for_task(task_type).default_command(&TaskFlags {
        profile: bp,
        path: crate::pb::PathContext::default(),
    });
    command == default
}

/// Rebuild the fingerprint canonical text from ResolvedProfile fields.
/// Ignores `profile.canonical` so a client cannot hash one thing and run another.
///
/// `egress` is the repository's *request* list (not the server grant); the grant
/// is folded separately via `fingerprint::with_egress`.
pub fn canonicalize_resolved(
    profile: &crate::pb::ResolvedProfile,
    task_type: TaskType,
    command: &str,
    egress: &[String],
) -> String {
    let mut s = String::new();
    let mut push = |k: &str, v: &str| {
        s.push_str(k);
        s.push('=');
        s.push_str(v);
        s.push('\n');
    };
    push("adapter", &profile.adapter);
    push("image", &profile.image);
    push("toolchain", &profile.toolchain);
    push("path", &profile.path);
    push("target", &profile.target);
    push(
        "timeout_secs",
        &if profile.timeout_secs == 0 {
            crate::DEFAULT_TASK_TIMEOUT_SECS
        } else {
            profile.timeout_secs
        }
        .to_string(),
    );
    push("task_type", task_type.as_str());
    push("command", command);
    let mut features = profile.features.clone();
    features.sort();
    features.dedup();
    push("features", &features.join(","));
    let mut eg = egress.to_vec();
    eg.sort();
    eg.dedup();
    push("egress", &eg.join(","));
    for (i, c) in profile.pre_commands.iter().enumerate() {
        push(&format!("pre_commands[{i}]"), c);
    }
    // Env sorted by key for stability.
    let mut env: Vec<_> = profile.env.iter().collect();
    env.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in env {
        push(&format!("env[{k}]"), v);
    }
    s
}

/// Apply resolve_env + canonicalize to a wire profile. Returns the effective
/// profile that must be stored and sent to the worker.
pub fn effective_profile(
    profile: &crate::pb::ResolvedProfile,
    task_type: TaskType,
    command: &str,
    request_env: &BTreeMap<String, String>,
    egress: &[String],
    inject_contract_env: bool,
) -> Result<crate::pb::ResolvedProfile, EnvResolveError> {
    let contract = for_task(task_type);
    let profile_env: BTreeMap<String, String> =
        profile.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let defaults = if inject_contract_env {
        contract.default_env()
    } else {
        &[]
    };
    // Profile.env from the agent already contains contract defaults + profile
    // + prior request merges. We re-apply contract defaults under profile, then
    // request env on top (denylist only on request) so the server result does
    // not depend on the client-supplied canonical string.
    let effective_env = resolve_env(&BTreeMap::new(), defaults, &profile_env, request_env)?;
    let mut out = profile.clone();
    out.env = effective_env.into_iter().collect();
    out.canonical = canonicalize_resolved(&out, task_type, command, egress);
    Ok(out)
}

// ---- Check / Build --------------------------------------------------------

pub struct CheckContract;
pub struct BuildContract;

impl TaskContract for CheckContract {
    fn task_type(&self) -> TaskType {
        TaskType::Check
    }
    fn default_command(&self, flags: &TaskFlags) -> String {
        crate::scope::resolve_command(&flags.profile, TaskType::Check, &flags.path, "").command
    }
    fn default_env(&self) -> &[(&'static str, &'static str)] {
        &[]
    }
    fn parse_outcome(&self, exit_code: i32, _log: &str, _default: bool) -> TaskOutcome {
        if exit_code == 0 {
            TaskOutcome::Success {
                summary: "success".into(),
            }
        } else {
            TaskOutcome::Custom { exit_code }
        }
    }
    fn render_outcome(&self, o: &TaskOutcome) -> ConclusionParts {
        match o {
            TaskOutcome::Success { summary } => ConclusionParts {
                headline: format!("✓ {summary}"),
                details: vec![],
            },
            TaskOutcome::Custom { exit_code } => ConclusionParts {
                headline: format!("✗ exit {exit_code}"),
                details: vec![],
            },
            _ => ConclusionParts {
                headline: "✗".into(),
                details: vec![],
            },
        }
    }
    fn remediation(&self, rule: &str) -> Option<Remediation> {
        resource_remediation(rule)
    }
}

impl TaskContract for BuildContract {
    fn task_type(&self) -> TaskType {
        TaskType::Build
    }
    fn default_command(&self, flags: &TaskFlags) -> String {
        crate::scope::resolve_command(&flags.profile, TaskType::Build, &flags.path, "").command
    }
    fn default_env(&self) -> &[(&'static str, &'static str)] {
        &[]
    }
    fn parse_outcome(&self, exit_code: i32, _log: &str, _default: bool) -> TaskOutcome {
        if exit_code == 0 {
            TaskOutcome::Success {
                summary: "success".into(),
            }
        } else {
            TaskOutcome::Custom { exit_code }
        }
    }
    fn render_outcome(&self, o: &TaskOutcome) -> ConclusionParts {
        CheckContract.render_outcome(o)
    }
    fn remediation(&self, rule: &str) -> Option<Remediation> {
        resource_remediation(rule)
    }
}

// ---- Test -----------------------------------------------------------------

/// Default env for `cargo test`: strip debuginfo. Intentional product decision
/// (feedback #2) — backtraces lose line detail; override via profile/request env.
pub const TEST_DEFAULT_ENV: &[(&str, &str)] = &[
    ("CARGO_PROFILE_TEST_DEBUG", "0"),
    ("CARGO_PROFILE_DEV_DEBUG", "0"),
];

pub struct TestContract;

impl TaskContract for TestContract {
    fn task_type(&self) -> TaskType {
        TaskType::Test
    }
    fn default_command(&self, flags: &TaskFlags) -> String {
        crate::scope::resolve_command(&flags.profile, TaskType::Test, &flags.path, "").command
    }
    fn default_env(&self) -> &[(&'static str, &'static str)] {
        TEST_DEFAULT_ENV
    }
    fn parse_outcome(&self, exit_code: i32, log: &str, command_is_default: bool) -> TaskOutcome {
        // Parser only when the command is the contract default (F24).
        if !command_is_default {
            return TaskOutcome::Custom { exit_code };
        }
        let summary = diag::parse_test_summary(log);
        if exit_code == 0 && summary.summary_seen && summary.failed == 0 {
            return TaskOutcome::Test {
                summary,
                parsed: true,
            };
        }
        if summary.summary_seen {
            return TaskOutcome::Test {
                summary,
                parsed: true,
            };
        }
        if exit_code == 0 {
            TaskOutcome::Success {
                summary: "success".into(),
            }
        } else {
            TaskOutcome::Custom { exit_code }
        }
    }
    fn render_outcome(&self, o: &TaskOutcome) -> ConclusionParts {
        match o {
            TaskOutcome::Test { summary, parsed } if *parsed && summary.summary_seen => {
                if summary.failed == 0 {
                    ConclusionParts {
                        headline: format!(
                            "✓ 测试通过：{} passed, {} ignored（{} 个二进制）",
                            summary.passed, summary.ignored, summary.binaries
                        ),
                        details: vec![],
                    }
                } else {
                    let mut details = vec![];
                    for n in summary.failed_names.iter().take(20) {
                        details.push(format!("  - {n}"));
                    }
                    if !summary.summary_seen {
                        details.push("未识别到测试摘要".into());
                    }
                    ConclusionParts {
                        headline: format!(
                            "✗ 测试失败：{} passed, {} failed, {} ignored（{} 个二进制）",
                            summary.passed, summary.failed, summary.ignored, summary.binaries
                        ),
                        details,
                    }
                }
            }
            TaskOutcome::Success { summary } => ConclusionParts {
                headline: format!("✓ {summary}"),
                details: vec![],
            },
            TaskOutcome::Custom { exit_code } => ConclusionParts {
                headline: format!("✗ exit {exit_code}（未识别到测试摘要）"),
                details: vec![],
            },
            _ => ConclusionParts {
                headline: "✗".into(),
                details: vec![],
            },
        }
    }
    fn remediation(&self, rule: &str) -> Option<Remediation> {
        resource_remediation(rule)
    }
}

// ---- Custom ---------------------------------------------------------------

pub struct CustomContract;

impl TaskContract for CustomContract {
    fn task_type(&self) -> TaskType {
        TaskType::Custom
    }
    fn default_command(&self, _flags: &TaskFlags) -> String {
        "true".into()
    }
    fn default_env(&self) -> &[(&'static str, &'static str)] {
        &[]
    }
    fn parse_outcome(&self, exit_code: i32, _log: &str, _default: bool) -> TaskOutcome {
        TaskOutcome::Custom { exit_code }
    }
    fn render_outcome(&self, o: &TaskOutcome) -> ConclusionParts {
        match o {
            TaskOutcome::Custom { exit_code: 0 } | TaskOutcome::Success { .. } => ConclusionParts {
                headline: "✓ success".into(),
                details: vec![],
            },
            TaskOutcome::Custom { exit_code } => ConclusionParts {
                headline: format!("✗ exit {exit_code}"),
                details: vec![],
            },
            _ => ConclusionParts {
                headline: "✗".into(),
                details: vec![],
            },
        }
    }
    fn remediation(&self, rule: &str) -> Option<Remediation> {
        resource_remediation(rule)
    }
}

// ---- env resolution (unique path for agent + server) ----------------------

/// Keys that request-level env must not set. Profile env is unrestricted so
/// existing sccache overrides keep working (F11.2).
pub const REQUEST_ENV_DENYLIST: &[&str] = &[
    "PATH",
    "HOME",
    "RUSTC_WRAPPER",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "CARGO_TARGET_DIR",
];

fn is_denied_request_key(key: &str) -> bool {
    if REQUEST_ENV_DENYLIST.iter().any(|k| *k == key) {
        return true;
    }
    key.starts_with("SCCACHE_")
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Debug, Clone)]
pub struct EnvResolveError {
    pub message: String,
}

impl std::fmt::Display for EnvResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EnvResolveError {}

/// Layered merge: adapter defaults < contract default_env < profile env <
/// request env. Denylist and shape checks apply only to request env.
///
/// The returned map is the effective env that must be written into
/// `ResolvedProfile.env` before canonicalize/fingerprint.
pub fn resolve_env(
    adapter_defaults: &BTreeMap<String, String>,
    contract_defaults: &[(&str, &str)],
    profile_env: &BTreeMap<String, String>,
    request_env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, EnvResolveError> {
    let mut out = BTreeMap::new();
    for (k, v) in adapter_defaults {
        out.insert(k.clone(), v.clone());
    }
    for (k, v) in contract_defaults {
        out.insert((*k).to_string(), (*v).to_string());
    }
    for (k, v) in profile_env {
        out.insert(k.clone(), v.clone());
    }
    if request_env.len() > 32 {
        return Err(EnvResolveError {
            message: format!("request env has {} entries (max 32)", request_env.len()),
        });
    }
    for (k, v) in request_env {
        if !valid_env_key(k) {
            return Err(EnvResolveError {
                message: format!("invalid env key `{k}` (must match ^[A-Za-z_][A-Za-z0-9_]*$)"),
            });
        }
        if is_denied_request_key(k) {
            return Err(EnvResolveError {
                message: format!("request env key `{k}` is denied"),
            });
        }
        if v.len() > 4 * 1024 {
            return Err(EnvResolveError {
                message: format!("request env value for `{k}` exceeds 4KB"),
            });
        }
        out.insert(k.clone(), v.clone());
    }
    Ok(out)
}

/// Closed auto-remediation whitelist (F3/F8).
pub fn auto_remediate_allowed(rule: &str) -> bool {
    matches!(rule, "oom_killed" | "sigkill_suspected_oom")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contract_injects_debug_zero() {
        let c = TestContract;
        let env: BTreeMap<_, _> = c.default_env().iter().cloned().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert_eq!(env["CARGO_PROFILE_TEST_DEBUG"], "0");
        assert_eq!(env["CARGO_PROFILE_DEV_DEBUG"], "0");
    }

    #[test]
    fn resolve_env_layers_request_over_contract() {
        let contract = TestContract.default_env();
        let profile = BTreeMap::from([("RUSTFLAGS".into(), "-C opt-level=1".into())]);
        let request = BTreeMap::from([("CARGO_PROFILE_TEST_DEBUG".into(), "2".into())]);
        let eff = resolve_env(&BTreeMap::new(), contract, &profile, &request).unwrap();
        assert_eq!(eff["CARGO_PROFILE_TEST_DEBUG"], "2"); // request wins
        assert_eq!(eff["CARGO_PROFILE_DEV_DEBUG"], "0");
        assert_eq!(eff["RUSTFLAGS"], "-C opt-level=1");
    }

    #[test]
    fn resolve_env_denies_path_in_request() {
        let request = BTreeMap::from([("PATH".into(), "/evil".into())]);
        assert!(resolve_env(&BTreeMap::new(), &[], &BTreeMap::new(), &request).is_err());
    }

    #[test]
    fn profile_may_set_sccache_but_request_may_not() {
        let profile = BTreeMap::from([("SCCACHE_DIR".into(), "/tmp".into())]);
        assert!(resolve_env(&BTreeMap::new(), &[], &profile, &BTreeMap::new()).is_ok());
        let request = BTreeMap::from([("SCCACHE_DIR".into(), "/tmp".into())]);
        assert!(resolve_env(&BTreeMap::new(), &[], &BTreeMap::new(), &request).is_err());
    }

    #[test]
    fn test_parser_disabled_when_command_overridden() {
        let c = TestContract;
        let log = "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out";
        match c.parse_outcome(1, log, false) {
            TaskOutcome::Custom { exit_code: 1 } => {}
            other => panic!("expected Custom, got {other:?}"),
        }
        match c.parse_outcome(1, log, true) {
            TaskOutcome::Test { summary, parsed: true } => assert_eq!(summary.failed, 1),
            other => panic!("expected Test, got {other:?}"),
        }
    }

    #[test]
    fn remediation_only_for_oom_rules() {
        let c = TestContract;
        let r = c.remediation("oom_killed").expect("oom");
        assert!(r.env_patch.iter().any(|(k, v)| k == "CARGO_BUILD_JOBS" && v == "2"));
        assert!(r.note.contains("CARGO_BUILD_JOBS=2"));
        assert!(c.remediation("sigkill_suspected_oom").is_some());
        assert!(c.remediation("disk_full").is_none());
        assert!(c.remediation("timeout").is_none());
        assert!(auto_remediate_allowed("oom_killed"));
        assert!(!auto_remediate_allowed("disk_full"));
    }

    #[test]
    fn command_is_default_gate() {
        let empty = std::collections::HashMap::new();
        assert!(command_is_default(TaskType::Test, "", &empty));
        assert!(!command_is_default(TaskType::Test, "cargo nextest", &empty));
        assert!(!command_is_default(TaskType::Build, "", &empty));
        let mut tasks = std::collections::HashMap::new();
        tasks.insert("test".into(), "cargo nextest run".into());
        assert!(!command_is_default(TaskType::Test, "", &tasks));
    }

    #[test]
    fn lying_canonical_is_ignored_by_effective_profile() {
        let mut profile = crate::pb::ResolvedProfile {
            adapter: "rust".into(),
            image: "reg/env@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            toolchain: "rustc 1.85.0".into(),
            canonical: "command=LIE\nenv[X]=Y\n".into(),
            env: Default::default(),
            ..Default::default()
        };
        profile.env.insert("FOO".into(), "1".into());
        let request = BTreeMap::from([("BAR".into(), "2".into())]);
        let eff = effective_profile(
            &profile,
            TaskType::Check,
            "cargo check",
            &request,
            &[],
            true,
        )
        .unwrap();
        assert!(!eff.canonical.contains("LIE"));
        assert!(eff.canonical.contains("command=cargo check"));
        assert!(eff.canonical.contains("env[BAR]=2"));
        assert!(eff.canonical.contains("env[FOO]=1"));
        // Different request env → different fingerprint input.
        let request2 = BTreeMap::from([("BAR".into(), "3".into())]);
        let eff2 = effective_profile(
            &profile,
            TaskType::Check,
            "cargo check",
            &request2,
            &[],
            true,
        )
        .unwrap();
        assert_ne!(eff.canonical, eff2.canonical);
        assert_eq!(eff2.env.get("BAR").map(|s| s.as_str()), Some("3"));
    }

    #[test]
    fn noop_remediation_patch_is_detectable() {
        // Mechanical guard: if first env already has JOBS=2, patch is no-op.
        let rem = TestContract.remediation("oom_killed").unwrap();
        let first = BTreeMap::from([("CARGO_BUILD_JOBS".into(), "2".into())]);
        let mut patched = first.clone();
        for (k, v) in &rem.env_patch {
            patched.insert(k.clone(), v.clone());
        }
        assert_eq!(first, patched);
    }
}
