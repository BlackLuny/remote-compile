//! Finding the directories outside a repository that its build actually needs.
//!
//! A cargo workspace can reach outside itself in several ways, and missing any
//! of them is the worst possible failure: the remote build uses code the
//! developer did not write, or fails on a file that exists locally, with
//! nothing in the diff to explain it (§4.3). So discovery is **fail-closed** —
//! when it cannot promise completeness it says so, and the caller refuses to
//! build rather than producing a plausible answer from partial inputs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How far to follow path dependencies of path dependencies. Each hop is a
/// separate `cargo metadata` invocation, and real projects do not nest deeply.
pub const MAX_DEPTH: usize = 4;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Discovery {
    /// Canonical directories outside the starting root, sorted and deduplicated.
    pub roots: Vec<PathBuf>,
    /// False when something prevented a full answer. The caller must not build.
    pub complete: bool,
    /// Why it is incomplete, or what was skipped. Always shown to the user.
    pub notes: Vec<String>,
}

impl Discovery {
    pub fn complete(roots: Vec<PathBuf>) -> Self {
        Discovery { roots, complete: true, notes: Vec::new() }
    }

    pub fn incomplete(note: impl Into<String>) -> Self {
        Discovery { roots: Vec::new(), complete: false, notes: vec![note.into()] }
    }
}

/// Every path dependency a cargo workspace reaches, transitively.
///
/// `root` is the repository root. Results are canonical directories; ones
/// inside `root` are still reported, because "inside the repository" does not
/// mean "enumerated by the repository" — a `.gitignore`d directory is neither,
/// and the caller decides that by checking actual coverage.
pub fn cargo_path_dependencies(root: &Path) -> Discovery {
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    let mut seen_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    // Keyed by *workspace*, not directory. Every member of a workspace reports
    // that whole workspace's metadata, so visiting members individually would
    // rediscover all their siblings and count another level of depth each time
    // — a 24-member workspace exhausts the depth limit without ever leaving
    // itself.
    let mut visited_workspaces: BTreeSet<PathBuf> = BTreeSet::new();
    let mut notes: Vec<String> = Vec::new();
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, depth)) = queue.pop() {
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        if depth > MAX_DEPTH {
            notes.push(format!(
                "stopped following path dependencies at {} after {MAX_DEPTH} levels",
                dir.display()
            ));
            return Discovery { roots: Vec::new(), complete: false, notes };
        }

        let workspace = match cargo_metadata(&dir) {
            Ok(w) => w,
            Err(e) => {
                notes.push(format!("cargo metadata failed in {}: {e}", dir.display()));
                return Discovery { roots: Vec::new(), complete: false, notes };
            }
        };
        if !visited_workspaces.insert(workspace.workspace_root.clone()) {
            continue;
        }

        // Patches and replacements are only honoured at the workspace root, so
        // that is the only manifest worth reading them from.
        let mut candidates = workspace.path_deps;
        match manifest_overrides(&workspace.workspace_root) {
            Ok(more) => candidates.extend(more),
            Err(e) => {
                notes.push(format!(
                    "could not read [patch]/[replace] from {}: {e}",
                    workspace.workspace_root.display()
                ));
                return Discovery { roots: Vec::new(), complete: false, notes };
            }
        }
        match config_patches(&workspace.workspace_root) {
            Ok(more) => candidates.extend(more),
            Err(e) => {
                notes.push(format!(
                    "could not read .cargo/config.toml patches under {}: {e}",
                    workspace.workspace_root.display()
                ));
                return Discovery { roots: Vec::new(), complete: false, notes };
            }
        }

        let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        for candidate in candidates {
            let Ok(canonical) = candidate.canonicalize() else {
                // A path dependency whose target does not exist yet — generated
                // by a pre_command, or simply a broken manifest. Either way the
                // set of roots cannot be settled now.
                notes.push(format!(
                    "path dependency {} does not exist; cannot determine the full root set",
                    candidate.display()
                ));
                return Discovery { roots: Vec::new(), complete: false, notes };
            };
            if !canonical.is_dir() {
                continue;
            }
            // Cargo resolves `../lib` lexically; we scan and mount the canonical
            // path. Usually those agree up to a shared prefix — `/var` being a
            // symlink to `/private/var` shifts every root equally and changes
            // nothing. What breaks the layout is a symlink *between* the roots,
            // e.g. `/ws/lib -> /opt/lib`: cargo goes on looking for `../lib`
            // while the mount lands under `/opt`. Compare the two offsets, not
            // the two paths.
            let lexical_offset = crate::roots::relative_path(&dir, &lexical(&candidate));
            let canonical_offset = crate::roots::relative_path(&canonical_dir, &canonical);
            if lexical_offset != canonical_offset {
                notes.push(format!(
                    "{} sits at `{lexical_offset}` but resolves to `{canonical_offset}` \
                     ({}); the container layout reproduces relative paths literally, so a \
                     symlinked path dependency cannot be represented",
                    candidate.display(),
                    canonical.display()
                ));
                return Discovery { roots: Vec::new(), complete: false, notes };
            }
            // A path dependency names a *package* directory, which may be a
            // member of a larger workspace. Syncing the package alone leaves
            // cargo without the workspace manifest it inherits from, so the
            // root that matters is the workspace's.
            let owner = match cargo_metadata(&canonical) {
                Ok(w) => w.workspace_root.canonicalize().unwrap_or(w.workspace_root),
                Err(e) => {
                    notes.push(format!(
                        "cannot determine the workspace owning {}: {e}",
                        canonical.display()
                    ));
                    return Discovery { roots: Vec::new(), complete: false, notes };
                }
            };
            found.insert(owner.clone());
            let canonical = owner;
            // A member of a workspace already analysed adds nothing new.
            let already_covered = visited_workspaces
                .iter()
                .any(|ws| canonical.starts_with(ws));
            if !already_covered && !seen_dirs.contains(&canonical) {
                queue.push((canonical, depth + 1));
            }
        }
    }

    Discovery { roots: found.into_iter().collect(), complete: true, notes }
}

/// Resolve `.` and `..` textually, the way cargo reads a `path` value — without
/// consulting the filesystem, so symlinks are not followed.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

struct WorkspaceMeta {
    workspace_root: PathBuf,
    path_deps: Vec<PathBuf>,
}

/// `--no-deps` keeps this to the workspace's own members: no registry
/// resolution, so it is fast and works offline. Both sources matter:
///
/// * `packages[].dependencies[].path` — cargo has already resolved these to
///   absolute paths, including ones inherited from `[workspace.dependencies]`
///   and ones declared only for a specific target;
/// * `packages[].manifest_path` outside the root — a workspace can list members
///   outside its own directory (`members = ["../plugins/*"]`), and such a
///   member need not be anybody's dependency.
fn cargo_metadata(dir: &Path) -> Result<WorkspaceMeta, String> {
    let manifest = dir.join("Cargo.toml");
    if !manifest.is_file() {
        return Err(format!("no Cargo.toml in {}", dir.display()));
    }
    let out = Command::new("cargo")
        .current_dir(dir)
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("unreadable metadata: {e}"))?;

    let workspace_root = json
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.to_path_buf());

    let mut path_deps = Vec::new();
    if let Some(packages) = json.get("packages").and_then(|v| v.as_array()) {
        for pkg in packages {
            if let Some(mp) = pkg.get("manifest_path").and_then(|v| v.as_str()) {
                if let Some(parent) = Path::new(mp).parent() {
                    if !parent.starts_with(dir) {
                        path_deps.push(parent.to_path_buf());
                    }
                }
            }
            let Some(deps) = pkg.get("dependencies").and_then(|v| v.as_array()) else {
                continue;
            };
            for dep in deps {
                if let Some(path) = dep.get("path").and_then(|v| v.as_str()) {
                    path_deps.push(PathBuf::from(path));
                }
            }
        }
    }
    Ok(WorkspaceMeta { workspace_root, path_deps })
}

/// `[patch.*]` and `[replace]` entries carrying a `path`. `cargo metadata` does
/// not report either, and a local patch is exactly the kind of thing someone
/// switches on temporarily while debugging a dependency — precisely when a
/// remote build silently using the unpatched version would mislead them most.
pub fn manifest_overrides(workspace_root: &Path) -> Result<Vec<PathBuf>, String> {
    let manifest = workspace_root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Ok(Vec::new());
    };
    let value: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(patch) = value.get("patch").and_then(|v| v.as_table()) {
        for (_registry, entries) in patch {
            collect_paths(entries, workspace_root, &mut out);
        }
    }
    if let Some(replace) = value.get("replace") {
        collect_paths(replace, workspace_root, &mut out);
    }
    Ok(out)
}

/// Cargo also accepts `[patch]` in `.cargo/config.toml`.
pub fn config_patches(workspace_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for name in [".cargo/config.toml", ".cargo/config"] {
        let path = workspace_root.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let value: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
        if let Some(patch) = value.get("patch").and_then(|v| v.as_table()) {
            for (_registry, entries) in patch {
                collect_paths(entries, workspace_root, &mut out);
            }
        }
    }
    Ok(out)
}

/// Pull `path = "..."` out of a table of dependency specifications, resolving
/// each relative to the manifest that declared it.
fn collect_paths(entries: &toml::Value, base: &Path, out: &mut Vec<PathBuf>) {
    let Some(table) = entries.as_table() else {
        return;
    };
    for (_name, spec) in table {
        if let Some(path) = spec.get("path").and_then(|v| v.as_str()) {
            let p = Path::new(path);
            out.push(if p.is_absolute() { p.to_path_buf() } else { base.join(p) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-disc-{tag}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn patch_paths_are_found_because_cargo_metadata_never_reports_them() {
        // A `[patch]` pointing at a local checkout is a routine debugging move.
        // Missing it means the remote build quietly uses the registry version.
        let ws = scratch("patch");
        write(
            &ws,
            "Cargo.toml",
            r#"
[workspace]
members = ["a"]

[patch.crates-io]
smoltcp = { path = "../smoltcp" }
jobserver = { git = "https://example.com/j", tag = "0.1.32" }
"#,
        );
        let found = manifest_overrides(&ws).unwrap();
        assert_eq!(found, vec![ws.join("../smoltcp")]);
    }

    #[test]
    fn a_patch_in_cargo_config_counts_too() {
        let ws = scratch("cfgpatch");
        write(
            &ws,
            ".cargo/config.toml",
            "[patch.crates-io]\nfoo = { path = \"../foo\" }\n",
        );
        assert_eq!(config_patches(&ws).unwrap(), vec![ws.join("../foo")]);
    }

    #[test]
    fn replace_entries_are_found() {
        let ws = scratch("replace");
        write(
            &ws,
            "Cargo.toml",
            "[workspace]\nmembers = []\n\n[replace]\n\"foo:0.1.0\" = { path = \"../foo\" }\n",
        );
        assert_eq!(manifest_overrides(&ws).unwrap(), vec![ws.join("../foo")]);
    }

    #[test]
    fn an_absolute_patch_path_is_taken_as_is() {
        let ws = scratch("abspatch");
        write(
            &ws,
            "Cargo.toml",
            "[workspace]\nmembers = []\n\n[patch.crates-io]\nfoo = { path = \"/opt/foo\" }\n",
        );
        assert_eq!(manifest_overrides(&ws).unwrap(), vec![PathBuf::from("/opt/foo")]);
    }

    #[test]
    fn a_manifest_without_overrides_yields_nothing() {
        let ws = scratch("plain");
        write(&ws, "Cargo.toml", "[package]\nname = \"x\"\nversion = \"0.1.0\"\n");
        assert!(manifest_overrides(&ws).unwrap().is_empty());
        assert!(config_patches(&ws).unwrap().is_empty());
    }

    #[test]
    fn a_broken_manifest_is_an_error_not_an_empty_answer() {
        // Returning "no extra roots" here would look exactly like a project
        // that has none, and the build would proceed with missing code.
        let ws = scratch("broken");
        write(&ws, "Cargo.toml", "[patch.crates-io\nbroken");
        assert!(manifest_overrides(&ws).is_err());
    }

    #[test]
    fn discovery_reports_incompleteness_rather_than_guessing() {
        let d = Discovery::incomplete("cargo metadata failed");
        assert!(!d.complete);
        assert!(d.roots.is_empty());
        assert_eq!(d.notes.len(), 1);
    }

    #[test]
    fn a_directory_without_a_manifest_cannot_be_analysed() {
        let d = scratch("nomanifest");
        let result = cargo_path_dependencies(&d);
        assert!(!result.complete, "no Cargo.toml means no promise of completeness");
    }

    #[test]
    fn a_real_workspace_with_a_sibling_path_dependency_is_discovered() {
        let base = scratch("ws");
        let app = base.join("app");
        let lib = base.join("lib");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&lib).unwrap();

        write(&lib, "Cargo.toml", "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        write(&lib, "src/lib.rs", "");
        write(
            &app,
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nlib = { path = \"../lib\" }\n",
        );
        write(&app, "src/main.rs", "fn main() {}");

        let found = cargo_path_dependencies(&app);
        assert!(found.complete, "notes: {:?}", found.notes);
        assert_eq!(
            found.roots,
            vec![lib.canonicalize().unwrap()],
            "the sibling crate must be discovered"
        );
    }

    #[test]
    fn a_workspace_with_many_members_does_not_exhaust_the_depth_limit() {
        // Every member reports the *whole* workspace's metadata, so walking
        // members as if they were separate workspaces rediscovers all their
        // siblings and burns a level of depth each time. A workspace with more
        // members than MAX_DEPTH then fails for no reason.
        let base = scratch("members");
        let ws = base.join("ws");
        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        write(&lib, "Cargo.toml", "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        write(&lib, "src/lib.rs", "");

        let names: Vec<String> = (0..MAX_DEPTH + 3).map(|i| format!("m{i}")).collect();
        let members = names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
        write(&ws, "Cargo.toml", &format!("[workspace]\nresolver = \"2\"\nmembers = [{members}]\n"));
        for (i, name) in names.iter().enumerate() {
            // Chain each member onto the previous one, so there is plenty for a
            // depth-counting walk to trip over.
            let prev = if i == 0 {
                "lib = { path = \"../../lib\" }".to_string()
            } else {
                format!("m{} = {{ path = \"../m{}\" }}", i - 1, i - 1)
            };
            write(
                &ws,
                &format!("{name}/Cargo.toml"),
                &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{prev}\n"),
            );
            write(&ws, &format!("{name}/src/lib.rs"), "");
        }

        let found = cargo_path_dependencies(&ws);
        assert!(found.complete, "notes: {:?}", found.notes);
        assert!(
            found.roots.contains(&lib.canonicalize().unwrap()),
            "the external crate must still be found: {:?}",
            found.roots
        );
    }

    #[test]
    fn a_path_dependency_that_does_not_exist_fails_closed() {
        let app = scratch("missingdep");
        write(
            &app,
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nghost = { path = \"../ghost\" }\n",
        );
        write(&app, "src/main.rs", "fn main() {}");

        let found = cargo_path_dependencies(&app);
        assert!(!found.complete, "a missing target means the root set is unknown");
    }
}
