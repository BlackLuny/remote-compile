//! Language adapters (§10).
//!
//! An adapter knows four things a generic runner cannot: what a project of
//! this language looks like, what "check it" means, how to read its error
//! output, and which caches make it fast.

use crate::model::TaskType;
use crate::pb::{Diagnostic, ResolvedProfile};
use crate::profile::BuildProfile;
use std::collections::BTreeMap;
use std::path::Path;

/// Mount points the worker must provide for a given adapter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheConfig {
    /// Environment injected into the build container.
    pub env: BTreeMap<String, String>,
    /// Per-(project, worktree) volume — the compile output directory.
    pub target_mount: Option<String>,
    /// Per-project volume — downloaded dependency sources.
    pub registry_mount: Option<String>,
    /// Worker-wide volume holding rustup's toolchains.
    ///
    /// `rust-toolchain.toml` pins a version per project, and the container root
    /// is read-only (§7.1), so a project asking for anything but the one baked
    /// into the image cannot install it — rustup fails on
    /// `/usr/local/rustup: Read-only file system`. Toolchains are large,
    /// identical across projects and immutable once written, so one volume for
    /// the whole worker is the right granularity.
    pub rustup_mount: Option<String>,
    /// Mount the host sccache unix socket (§7.2: the container is
    /// `--network=none`, so the daemon lives on the host and credentials for
    /// the remote cache backend never enter untrusted code).
    pub sccache_uds: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandSpec {
    /// Shell line executed inside the workspace directory.
    pub line: String,
    /// stdout carries machine-readable diagnostics (do not merge with stderr).
    pub json_stdout: bool,
}

pub trait Adapter: Send + Sync {
    fn name(&self) -> &'static str;

    /// Auto-detect a starting profile (§3.2 priority 4).
    fn detect(&self, repo_root: &Path) -> Option<BuildProfile>;

    /// Directories excluded from the sync scan.
    fn default_exclude(&self) -> &[&str];

    /// Command for a task type, honouring profile overrides.
    fn command_for(&self, profile: &BuildProfile, task: TaskType) -> CommandSpec;

    fn parse_diagnostics(&self, stdout: &str, stderr: &str) -> Vec<Diagnostic>;

    fn cache_config(&self, profile: &ResolvedProfile) -> CacheConfig;

    /// Host env vars absorbed during profile resolution. They land in the
    /// resolved profile and therefore in the fingerprint (§5.1) — a build
    /// influenced by `RUSTFLAGS` must not reuse a result computed without it.
    fn relevant_env(&self) -> &[&str];

    /// Default image when the fleet has no better idea.
    fn default_image(&self) -> &'static str;

    /// Directories outside `repo_root` that the build reads from.
    ///
    /// Most languages resolve dependencies from a registry or a vendor
    /// directory inside the repository and have nothing to report. Cargo does
    /// not: a `path` dependency may point anywhere, and the literal in
    /// `Cargo.toml` has to keep meaning the same thing remotely.
    fn extra_roots(&self, repo_root: &Path) -> crate::discover::Discovery {
        let _ = repo_root;
        crate::discover::Discovery::complete(Vec::new())
    }
}

pub fn for_name(name: &str) -> Box<dyn Adapter> {
    match name {
        "rust" => Box::new(RustAdapter),
        _ => Box::new(GenericAdapter),
    }
}

/// Try every adapter; first match wins.
pub fn detect(repo_root: &Path) -> Option<(String, BuildProfile)> {
    let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(RustAdapter)];
    for a in adapters {
        if let Some(p) = a.detect(repo_root) {
            return Some((a.name().to_string(), p));
        }
    }
    None
}

// ------------------------------- Rust -------------------------------

pub struct RustAdapter;

const RUST_EXCLUDE: &[&str] = &["target", ".git", "node_modules"];
const RUST_RELEVANT_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_PROFILE_DEV_DEBUG",
    "RUST_LOG",
];

impl Adapter for RustAdapter {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn detect(&self, repo_root: &Path) -> Option<BuildProfile> {
        if !repo_root.join("Cargo.toml").is_file() {
            return None;
        }
        let mut p = BuildProfile {
            adapter: Some("rust".into()),
            ..Default::default()
        };
        if let Some(tc) = read_toolchain(repo_root) {
            p.toolchain = Some(tc);
        }
        if let Some(t) = read_default_target(repo_root) {
            p.target = Some(t);
        }
        Some(p)
    }

    fn default_exclude(&self) -> &[&str] {
        RUST_EXCLUDE
    }

    fn command_for(&self, profile: &BuildProfile, task: TaskType) -> CommandSpec {
        if let Some(custom) = profile.tasks.get(task.as_str()) {
            // A custom command may or may not emit JSON; assume it does not,
            // and fall back to text parsing.
            return CommandSpec {
                line: custom.clone(),
                json_stdout: custom.contains("--message-format=json"),
            };
        }
        let mut flags = String::new();
        if let Some(t) = &profile.target {
            if !t.is_empty() {
                flags.push_str(&format!(" --target {t}"));
            }
        }
        if let Some(f) = &profile.features {
            if !f.is_empty() {
                flags.push_str(&format!(" --features {}", f.join(",")));
            }
        }
        let line = match task {
            TaskType::Check => format!("cargo check --workspace --all-targets --message-format=json{flags}"),
            TaskType::Build => format!("cargo build --workspace --message-format=json{flags}"),
            TaskType::Clippy => format!("cargo clippy --workspace --all-targets --message-format=json{flags} -- -D warnings"),
            TaskType::Test => format!("cargo test --workspace{flags}"),
            TaskType::Custom => "cargo check --workspace --message-format=json".to_string(),
        };
        CommandSpec {
            json_stdout: line.contains("--message-format=json"),
            line,
        }
    }

    fn parse_diagnostics(&self, stdout: &str, stderr: &str) -> Vec<Diagnostic> {
        let mut d = crate::diag::parse_cargo_json(stdout);
        if d.is_empty() {
            // `cargo test` and custom commands have no JSON stream.
            d = crate::diag::parse_generic(stderr);
        }
        d
    }

    fn cache_config(&self, profile: &ResolvedProfile) -> CacheConfig {
        let mut env = BTreeMap::new();
        env.insert("RUSTC_WRAPPER".into(), "sccache".into());
        // sccache and incremental compilation are mutually exclusive (§7.2):
        // dependencies ride sccache, local crates ride the target volume.
        env.insert("CARGO_INCREMENTAL".into(), "0".into());
        env.insert("CARGO_TERM_COLOR".into(), "never".into());
        env.insert("CARGO_HOME".into(), "/rc/cargo".into());
        env.insert("RUSTUP_HOME".into(), "/rc/rustup".into());
        env.insert("SCCACHE_SERVER_UDS".into(), "/rc/sccache/sccache.sock".into());
        for (k, v) in &profile.env {
            env.insert(k.clone(), v.clone());
        }
        CacheConfig {
            env,
            target_mount: Some("/rc/target".into()),
            registry_mount: Some("/rc/cargo".into()),
            rustup_mount: Some("/rc/rustup".into()),
            sccache_uds: true,
        }
    }

    fn relevant_env(&self) -> &[&str] {
        RUST_RELEVANT_ENV
    }

    fn default_image(&self) -> &'static str {
        "docker.io/library/rust:1-bookworm"
    }

    fn extra_roots(&self, repo_root: &Path) -> crate::discover::Discovery {
        crate::discover::cargo_path_dependencies(repo_root)
    }
}

fn read_toolchain(root: &Path) -> Option<String> {
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        let path = root.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if name == "rust-toolchain" && !text.contains('=') {
            let t = text.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        if let Ok(v) = toml::from_str::<toml::Value>(&text) {
            if let Some(c) = v.get("toolchain").and_then(|t| t.get("channel")).and_then(|c| c.as_str()) {
                return Some(c.to_string());
            }
        }
    }
    None
}

fn read_default_target(root: &Path) -> Option<String> {
    for rel in [".cargo/config.toml", ".cargo/config"] {
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        if let Ok(v) = toml::from_str::<toml::Value>(&text) {
            if let Some(t) = v.get("build").and_then(|b| b.get("target")).and_then(|t| t.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Heuristic scan of build scripts for system dependencies. Used to pre-fill
/// a `prepare_env` suggestion so the agent does not discover them one failed
/// build at a time.
pub fn system_dep_hints(root: &Path) -> Vec<String> {
    const NEEDLES: &[(&str, &str)] = &[
        ("protobuf_src", "protobuf-compiler"),
        ("prost-build", "protobuf-compiler"),
        ("tonic-build", "protobuf-compiler"),
        ("protoc", "protobuf-compiler"),
        ("pkg-config", "pkg-config"),
        ("openssl-sys", "libssl-dev pkg-config"),
        ("libsqlite3-sys", "libsqlite3-dev"),
        ("libgit2-sys", "libgit2-dev"),
        ("cmake", "cmake"),
        ("bindgen", "libclang-dev"),
        ("cc::Build", "build-essential"),
    ];
    let mut found: Vec<String> = Vec::new();
    let mut sources = Vec::new();
    for f in ["Cargo.toml", "build.rs", "Cargo.lock"] {
        if let Ok(t) = std::fs::read_to_string(root.join(f)) {
            sources.push(t);
        }
    }
    let haystack = sources.join("\n");
    for (needle, pkg) in NEEDLES {
        if haystack.contains(needle) && !found.iter().any(|f| f == pkg) {
            found.push((*pkg).to_string());
        }
    }
    found
}

// ------------------------------ generic ------------------------------

/// Fallback for projects with no recognised adapter: run whatever the profile
/// says and scrape the output (§10.3).
pub struct GenericAdapter;

const GENERIC_EXCLUDE: &[&str] = &["target", "build", "dist", ".git", "node_modules"];

impl Adapter for GenericAdapter {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn detect(&self, _repo_root: &Path) -> Option<BuildProfile> {
        None
    }

    fn default_exclude(&self) -> &[&str] {
        GENERIC_EXCLUDE
    }

    fn command_for(&self, profile: &BuildProfile, task: TaskType) -> CommandSpec {
        let line = profile
            .tasks
            .get(task.as_str())
            .cloned()
            .unwrap_or_else(|| "make check".to_string());
        CommandSpec { line, json_stdout: false }
    }

    fn parse_diagnostics(&self, stdout: &str, stderr: &str) -> Vec<Diagnostic> {
        let mut d = crate::diag::parse_generic(stderr);
        if d.is_empty() {
            d = crate::diag::parse_generic(stdout);
        }
        d
    }

    fn cache_config(&self, profile: &ResolvedProfile) -> CacheConfig {
        CacheConfig {
            env: profile.env.clone().into_iter().collect(),
            target_mount: None,
            registry_mount: None,
            rustup_mount: None,
            sccache_uds: false,
        }
    }

    fn relevant_env(&self) -> &[&str] {
        &[]
    }

    fn default_image(&self) -> &'static str {
        "docker.io/library/debian:bookworm-slim"
    }
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rc-adapter-{tag}-{}", ulid::Ulid::generate()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_a_cargo_project_and_reads_its_pins() {
        let d = scratch("rust");
        fs::write(d.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        fs::write(d.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.85.0\"\n").unwrap();
        fs::create_dir_all(d.join(".cargo")).unwrap();
        fs::write(d.join(".cargo/config.toml"), "[build]\ntarget = \"x86_64-unknown-linux-musl\"\n").unwrap();

        let p = RustAdapter.detect(&d).unwrap();
        assert_eq!(p.toolchain.as_deref(), Some("1.85.0"));
        assert_eq!(p.target.as_deref(), Some("x86_64-unknown-linux-musl"));
    }

    #[test]
    fn ignores_a_non_cargo_directory() {
        assert!(RustAdapter.detect(&scratch("empty")).is_none());
    }

    #[test]
    fn check_asks_for_json_diagnostics() {
        let spec = RustAdapter.command_for(&BuildProfile::default(), TaskType::Check);
        assert!(spec.line.contains("--message-format=json"));
        assert!(spec.json_stdout);
    }

    #[test]
    fn profile_task_overrides_the_default_command() {
        let mut p = BuildProfile::default();
        p.tasks.insert("test".into(), "cargo nextest run -p backend".into());
        let spec = RustAdapter.command_for(&p, TaskType::Test);
        assert_eq!(spec.line, "cargo nextest run -p backend");
        assert!(!spec.json_stdout);
    }

    #[test]
    fn target_and_features_reach_the_command_line() {
        let mut p = BuildProfile::default();
        p.target = Some("x86_64-unknown-linux-musl".into());
        p.features = Some(vec!["ssr".into()]);
        let spec = RustAdapter.command_for(&p, TaskType::Check);
        assert!(spec.line.contains("--target x86_64-unknown-linux-musl"));
        assert!(spec.line.contains("--features ssr"));
    }

    #[test]
    fn rust_cache_config_disables_incremental_for_sccache() {
        // §7.2 / risk #7: sccache and CARGO_INCREMENTAL cannot both be on.
        let cfg = RustAdapter.cache_config(&ResolvedProfile::default());
        assert_eq!(cfg.env["RUSTC_WRAPPER"], "sccache");
        assert_eq!(cfg.env["CARGO_INCREMENTAL"], "0");
        assert!(cfg.sccache_uds);
    }

    #[test]
    fn profile_env_overrides_adapter_defaults() {
        let mut rp = ResolvedProfile::default();
        rp.env.insert("RUSTC_WRAPPER".into(), "".into());
        let cfg = RustAdapter.cache_config(&rp);
        assert_eq!(cfg.env["RUSTC_WRAPPER"], "");
    }

    #[test]
    fn suggests_system_packages_from_build_inputs() {
        let d = scratch("hints");
        fs::write(d.join("Cargo.toml"), "[build-dependencies]\ntonic-build = \"0.14\"\n").unwrap();
        assert_eq!(system_dep_hints(&d), vec!["protobuf-compiler"]);
    }

    #[test]
    fn generic_adapter_falls_back_to_text_scraping() {
        let d = GenericAdapter.parse_diagnostics("", "main.c:3:1: error: oops");
        assert_eq!(d.len(), 1);
    }
}
