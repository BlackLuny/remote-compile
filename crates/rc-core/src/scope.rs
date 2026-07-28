//! Intent path → package scope resolution (intent-and-query-surface §3).
//!
//! Path is not only "find the repo root": it selects what default cargo
//! command runs (`-p <member>` vs `--workspace`). Execution and fingerprint
//! share one `resolve_command` path so Receipt cannot lie.

use crate::adapter;
use crate::model::TaskType;
use crate::pb::{self, PathContext, ScopeKind};
use crate::profile::BuildProfile;
use std::path::{Component, Path, PathBuf};

/// Workspace-default path context (root check, no package pin).
pub fn workspace_context(repo_root: &Path, intent_path: &Path) -> PathContext {
    PathContext {
        intent_path: intent_path.display().to_string(),
        repo_root: repo_root.display().to_string(),
        relative_path: String::new(),
        scope: ScopeKind::ScopeWorkspace as i32,
        packages: vec![],
        resolve_note: "workspace root or no package mapping".into(),
    }
}

/// Resolve `intent_path` under `repo_root` into a PathContext for the adapter.
pub fn resolve_path_context(repo_root: &Path, intent_path: &Path, adapter: &str) -> PathContext {
    let intent_canon = canonicalize_lossy(intent_path);
    let root_canon = canonicalize_lossy(repo_root);
    let relative = match intent_canon.strip_prefix(&root_canon) {
        Ok(r) => normalize_rel(r),
        Err(_) => {
            // intent outside root — still treat as workspace scope at root.
            return PathContext {
                intent_path: intent_path.display().to_string(),
                repo_root: repo_root.display().to_string(),
                relative_path: String::new(),
                scope: ScopeKind::ScopeWorkspace as i32,
                packages: vec![],
                resolve_note: "intent path outside repo root; using workspace scope".into(),
            };
        }
    };

    if adapter != "rust" {
        return PathContext {
            intent_path: intent_path.display().to_string(),
            repo_root: repo_root.display().to_string(),
            relative_path: relative,
            scope: ScopeKind::ScopeWorkspace as i32,
            packages: vec![],
            resolve_note: format!("adapter {adapter} has no package scope; workspace default"),
        };
    }

    let members = match rust_workspace_members(repo_root) {
        Ok(m) => m,
        Err(note) => {
            return PathContext {
                intent_path: intent_path.display().to_string(),
                repo_root: repo_root.display().to_string(),
                relative_path: relative,
                scope: ScopeKind::ScopeWorkspace as i32,
                packages: vec![],
                resolve_note: note,
            };
        }
    };

    if relative.is_empty() {
        return PathContext {
            intent_path: intent_path.display().to_string(),
            repo_root: repo_root.display().to_string(),
            relative_path: String::new(),
            scope: ScopeKind::ScopeWorkspace as i32,
            packages: vec![],
            resolve_note: "path is workspace root".into(),
        };
    }

    // Longest prefix match against member directories.
    // Root package (dir="") matches any relative path that is not under a
    // longer member directory — handled by preferring longer dirs first.
    let mut best: Option<&Member> = None;
    for m in &members {
        let matches = if m.dir.is_empty() {
            // root package: matches repo-relative files not claimed by a
            // longer member; still a candidate, length 0 loses to crates/foo.
            true
        } else {
            relative == m.dir || relative.starts_with(&(m.dir.clone() + "/"))
        };
        if matches {
            match best {
                None => best = Some(m),
                Some(b) if m.dir.len() > b.dir.len() => best = Some(m),
                _ => {}
            }
        }
    }

    match best {
        Some(m) => {
            // Self-check: package must still be on the member list.
            if !members.iter().any(|x| x.name == m.name) {
                return PathContext {
                    intent_path: intent_path.display().to_string(),
                    repo_root: repo_root.display().to_string(),
                    relative_path: relative,
                    scope: ScopeKind::ScopeWorkspace as i32,
                    packages: vec![],
                    resolve_note: format!(
                        "package {} failed member self-check; workspace fallback",
                        m.name
                    ),
                };
            }
            PathContext {
                intent_path: intent_path.display().to_string(),
                repo_root: repo_root.display().to_string(),
                relative_path: relative,
                scope: ScopeKind::ScopePackage as i32,
                packages: vec![m.name.clone()],
                resolve_note: format!("member {} ({})", m.name, m.dir),
            }
        }
        None => PathContext {
            intent_path: intent_path.display().to_string(),
            repo_root: repo_root.display().to_string(),
            relative_path: relative,
            scope: ScopeKind::ScopeWorkspace as i32,
            packages: vec![],
            resolve_note: "path did not match a workspace member; workspace fallback".into(),
        },
    }
}

/// Result of the single resolve_command entry point.
#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    pub command: String,
    pub json_stdout: bool,
    pub command_is_default: bool,
    pub path: PathContext,
    pub scope_hash: String,
}

/// Sole command synthesis path for agent and server (F1/F2).
///
/// Order: explicit override > profile.tasks > adapter default(path_context).
pub fn resolve_command(
    profile: &BuildProfile,
    task: TaskType,
    path: &PathContext,
    command_override: &str,
) -> ResolvedCommand {
    let adapter_name = profile.adapter.as_deref().unwrap_or("rust");
    let adapter = adapter::for_name(adapter_name);

    if !command_override.is_empty() {
        let mut pc = path.clone();
        pc.scope = ScopeKind::ScopeExplicitCommand as i32;
        pc.resolve_note = "explicit command override".into();
        let scope_hash = scope_hash_of(&pc, false, command_override);
        return ResolvedCommand {
            command: command_override.to_string(),
            json_stdout: command_override.contains("--message-format=json"),
            command_is_default: false,
            path: pc,
            scope_hash,
        };
    }

    if let Some(custom) = profile.tasks.get(task.as_str()) {
        let mut pc = path.clone();
        pc.scope = ScopeKind::ScopeProfileOverride as i32;
        let derived = path
            .packages
            .first()
            .map(|p| p.as_str())
            .unwrap_or("");
        if !derived.is_empty()
            && !custom.contains(&format!("-p {derived}"))
            && !custom.contains(&format!("--package {derived}"))
        {
            pc.resolve_note = format!(
                "profile task override; may not match path package {derived} (scope_mismatch)"
            );
        } else {
            pc.resolve_note = "profile [tasks] override".into();
        }
        let scope_hash = scope_hash_of(&pc, false, custom);
        return ResolvedCommand {
            command: custom.clone(),
            json_stdout: custom.contains("--message-format=json"),
            command_is_default: false,
            path: pc,
            scope_hash,
        };
    }

    let spec = adapter.command_for(profile, task, path);
    let mut pc = path.clone();
    // Preserve package/workspace; mark as default.
    if pc.scope == ScopeKind::ScopeUnspecified as i32 {
        pc.scope = ScopeKind::ScopeWorkspace as i32;
    }
    let scope_hash = scope_hash_of(&pc, true, &spec.line);
    ResolvedCommand {
        command: spec.line,
        json_stdout: spec.json_stdout,
        command_is_default: true,
        path: pc,
        scope_hash,
    }
}

/// Short hash for supersede / delta baseline keys.
pub fn scope_hash_of(path: &PathContext, command_is_default: bool, command: &str) -> String {
    let mut pkgs = path.packages.clone();
    pkgs.sort();
    let key = format!(
        "{}|{}|{}|{}",
        path.scope,
        pkgs.join(","),
        command_is_default,
        command.trim()
    );
    use blake3::Hasher;
    let mut h = Hasher::new();
    h.update(key.as_bytes());
    let hex = h.finalize().to_hex();
    hex[..12.min(hex.len())].to_string()
}

/// Wire PathContext from a SubmitTaskReq (empty → workspace).
pub fn path_context_from_pb(pb: Option<&PathContext>, fallback_root: &str) -> PathContext {
    match pb {
        Some(p) if !p.intent_path.is_empty() || p.scope != 0 || !p.packages.is_empty() => p.clone(),
        _ => PathContext {
            intent_path: fallback_root.into(),
            repo_root: fallback_root.into(),
            relative_path: String::new(),
            scope: ScopeKind::ScopeWorkspace as i32,
            packages: vec![],
            resolve_note: "missing path_context; workspace default".into(),
        },
    }
}

// ---- Rust workspace member discovery ------------------------------------

#[derive(Debug, Clone)]
struct Member {
    name: String,
    dir: String, // relative, '/' separated, no trailing slash
}

fn rust_workspace_members(repo_root: &Path) -> Result<Vec<Member>, String> {
    let cargo = repo_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&cargo)
        .map_err(|e| format!("cannot read Cargo.toml: {e}"))?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|e| format!("Cargo.toml parse error: {e}"))?;

    let ws = value.get("workspace");
    let member_globs: Vec<String> = ws
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let exclude_globs: Vec<String> = ws
        .and_then(|w| w.get("exclude"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Single-package crate (no workspace table).
    if member_globs.is_empty() {
        if let Some(name) = package_name_in(&value) {
            return Ok(vec![Member {
                name,
                dir: String::new(), // package at root
            }]);
        }
        return Err("no [workspace].members and no root [package]".into());
    }

    let mut dirs: Vec<String> = Vec::new();
    for pat in &member_globs {
        if pat.contains('*') {
            expand_glob(repo_root, pat, &exclude_globs, &mut dirs);
        } else {
            // Explicit members are NOT filtered by exclude (cargo semantics).
            let norm = pat.trim_matches('/').replace('\\', "/");
            if !norm.is_empty() {
                dirs.push(norm);
            }
        }
    }
    dirs.sort();
    dirs.dedup();

    let mut out = Vec::new();
    // Non-virtual workspace: root [package] is also a member.
    if let Some(name) = package_name_in(&value) {
        out.push(Member {
            name,
            dir: String::new(),
        });
    }
    for dir in dirs {
        let manifest = if dir.is_empty() {
            repo_root.join("Cargo.toml")
        } else {
            repo_root.join(&dir).join("Cargo.toml")
        };
        let Ok(mt) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(mv) = toml::from_str::<toml::Value>(&mt) else {
            continue;
        };
        if let Some(name) = package_name_in(&mv) {
            // Avoid duplicating root package if also listed as "".
            if !out.iter().any(|m| m.name == name && m.dir == dir) {
                out.push(Member { name, dir });
            }
        }
    }
    if out.is_empty() {
        return Err("workspace members expanded to zero packages".into());
    }
    Ok(out)
}

fn package_name_in(v: &toml::Value) -> Option<String> {
    v.get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(String::from)
}

/// Minimal glob: only trailing `/*` or `/**` and single `*` segment.
fn expand_glob(root: &Path, pat: &str, excludes: &[String], out: &mut Vec<String>) {
    let pat = pat.trim_matches('/').replace('\\', "/");
    // Simple cases: crates/*, crates/*/foo
    if let Some(prefix) = pat.strip_suffix("/*") {
        let base = root.join(prefix);
        let Ok(rd) = std::fs::read_dir(&base) else {
            return;
        };
        for ent in rd.flatten() {
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if is_excluded(&rel, excludes) {
                continue;
            }
            out.push(rel);
        }
        return;
    }
    // Fallback: treat as literal if not a known glob.
    if !pat.contains('*') {
        if !is_excluded(&pat, excludes) {
            out.push(pat);
        }
    }
}

fn is_excluded(rel: &str, excludes: &[String]) -> bool {
    for ex in excludes {
        let ex = ex.trim_matches('/').replace('\\', "/");
        if ex.contains('*') {
            // Only support prefix/* style.
            if let Some(prefix) = ex.strip_suffix("/*") {
                if rel == prefix || rel.starts_with(&(prefix.to_string() + "/")) {
                    // For crates/* exclude, match one segment under crates.
                    let rest = rel.strip_prefix(prefix).unwrap_or("").trim_start_matches('/');
                    if !rest.is_empty() && !rest.contains('/') {
                        return true;
                    }
                    if rel.starts_with(&(prefix.to_string() + "/")) {
                        return true;
                    }
                }
            }
        } else if rel == ex || rel.starts_with(&(ex.clone() + "/")) {
            return true;
        }
    }
    false
}

fn canonicalize_lossy(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| {
        // Best-effort normalize without requiring the path to exist fully.
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    })
}

fn normalize_rel(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.trim_matches('/').to_string()
}

/// Build cargo package selection args from PathContext for defaults.
pub fn package_flags(path: &PathContext) -> String {
    if path.scope != ScopeKind::ScopePackage as i32 || path.packages.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    for p in &path.packages {
        s.push_str(" -p ");
        s.push_str(p);
    }
    s
}

/// Whether path_context implies a package-scoped default (for scope_mismatch).
pub fn is_package_scope(path: &PathContext) -> bool {
    path.scope == ScopeKind::ScopePackage as i32 && !path.packages.is_empty()
}

/// Scope kind label for Receipt headlines.
pub fn scope_label(path: &PathContext) -> String {
    match ScopeKind::try_from(path.scope).unwrap_or(ScopeKind::ScopeUnspecified) {
        ScopeKind::ScopePackage => {
            if path.packages.len() == 1 {
                format!("package:{}", path.packages[0])
            } else {
                format!("package:{}", path.packages.join(","))
            }
        }
        ScopeKind::ScopeProfileOverride => "profile_override".into(),
        ScopeKind::ScopeExplicitCommand => "explicit_command".into(),
        ScopeKind::ScopeWorkspace => "workspace".into(),
        _ => "unspecified".into(),
    }
}

/// Convert ResolvedCommand into pb::EffectivePlan skeleton.
pub fn effective_plan_pb(
    resolved: &ResolvedCommand,
    task: &str,
    profile_source: &str,
    pre: Option<pb::PreCommandsStatus>,
) -> pb::EffectivePlan {
    pb::EffectivePlan {
        path: Some(resolved.path.clone()),
        task: task.into(),
        command: resolved.command.clone(),
        command_is_default: resolved.command_is_default,
        profile_source: profile_source.into(),
        pre_commands: pre,
        cache_key_note: format!("scope={}", scope_label(&resolved.path)),
        scope_hash: resolved.scope_hash.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-scope-{tag}-{}", ulid::Ulid::generate()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn package_path_selects_member() {
        let root = scratch("pkg");
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/a", "crates/b"]
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("crates/a/src")).unwrap();
        fs::create_dir_all(root.join("crates/b/src")).unwrap();
        fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname = \"pkg-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/b/Cargo.toml"),
            "[package]\nname = \"pkg-b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();

        let pc = resolve_path_context(&root, &root.join("crates/a/src/lib.rs"), "rust");
        assert_eq!(pc.scope, ScopeKind::ScopePackage as i32);
        assert_eq!(pc.packages, vec!["pkg-a".to_string()]);

        let mut profile = BuildProfile::default();
        profile.adapter = Some("rust".into());
        let r = resolve_command(&profile, TaskType::Check, &pc, "");
        assert!(r.command.contains("-p pkg-a"), "{}", r.command);
        assert!(!r.command.contains("--workspace"), "{}", r.command);
        assert!(r.command.contains("--all-targets"), "{}", r.command);
        assert!(r.command_is_default);
    }

    #[test]
    fn workspace_root_keeps_workspace_flag() {
        let root = scratch("ws");
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/a"]
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("crates/a")).unwrap();
        fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname = \"pkg-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        let pc = resolve_path_context(&root, &root, "rust");
        assert_eq!(pc.scope, ScopeKind::ScopeWorkspace as i32);
        let mut profile = BuildProfile::default();
        profile.adapter = Some("rust".into());
        let r = resolve_command(&profile, TaskType::Check, &pc, "");
        assert!(r.command.contains("--workspace"), "{}", r.command);
    }

    #[test]
    fn exclude_filters_glob_not_explicit() {
        let root = scratch("ex");
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]
exclude = ["crates/legacy"]
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("crates/good")).unwrap();
        fs::create_dir_all(root.join("crates/legacy")).unwrap();
        fs::write(
            root.join("crates/good/Cargo.toml"),
            "[package]\nname = \"good\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/legacy/Cargo.toml"),
            "[package]\nname = \"legacy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        let members = rust_workspace_members(&root).unwrap();
        assert!(members.iter().any(|m| m.name == "good"));
        assert!(!members.iter().any(|m| m.name == "legacy"));
    }

    #[test]
    fn explicit_command_not_rewritten() {
        let root = scratch("excmd");
        let pc = workspace_context(&root, &root);
        let profile = BuildProfile::default();
        let r = resolve_command(
            &profile,
            TaskType::Check,
            &pc,
            "cargo check -p foo --message-format=json",
        );
        assert!(!r.command_is_default);
        assert_eq!(r.command, "cargo check -p foo --message-format=json");
        assert_eq!(r.path.scope, ScopeKind::ScopeExplicitCommand as i32);
    }

    #[test]
    fn different_packages_different_scope_hash() {
        let mut a = workspace_context(Path::new("/r"), Path::new("/r/a"));
        a.scope = ScopeKind::ScopePackage as i32;
        a.packages = vec!["a".into()];
        let mut b = a.clone();
        b.packages = vec!["b".into()];
        assert_ne!(
            scope_hash_of(&a, true, "cargo check -p a"),
            scope_hash_of(&b, true, "cargo check -p b")
        );
    }

    #[test]
    fn profile_override_is_not_default() {
        let root = scratch("prof");
        let pc = workspace_context(&root, &root);
        let mut profile = BuildProfile::default();
        profile
            .tasks
            .insert("test".into(), "cargo nextest run -p backend".into());
        let r = resolve_command(&profile, TaskType::Test, &pc, "");
        assert!(!r.command_is_default);
        assert_eq!(r.command, "cargo nextest run -p backend");
    }
}
