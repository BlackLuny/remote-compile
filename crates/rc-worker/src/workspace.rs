//! Workspace reconstruction (§7.3).
//!
//! The manifest is the only source of truth. A file present in the git
//! baseline but absent from the manifest **must be deleted**: agents delete
//! files as often as they edit them, and a stale leftover produces the exact
//! "compiles remotely, fails locally" divergence §4.3 exists to prevent.

use anyhow::{anyhow, Result};
use rc_core::pb::{EntryType, FileEntry, Manifest};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Files the worker still needs, and files it must remove.
#[derive(Debug, Default, PartialEq)]
pub struct RebuildPlan {
    /// Entries whose content is not already correct on disk.
    pub fetch: Vec<FileEntry>,
    /// Absolute-safe relative paths to delete.
    pub delete: Vec<String>,
    /// Symlinks to (re)create: (path, target).
    pub symlinks: Vec<(String, String)>,
}

/// Compare the manifest against what is on disk.
pub fn plan(root: &Path, manifest: &Manifest) -> Result<RebuildPlan> {
    let mut plan = RebuildPlan::default();
    let mut wanted: HashSet<String> = HashSet::new();
    // Split out, because "this path is wanted" is not enough to decide it is
    // *correct*: a directory the manifest implies must be a real directory.
    let mut wanted_dirs: HashSet<String> = HashSet::new();
    let mut wanted_entries: HashSet<String> = HashSet::new();

    for entry in &manifest.entries {
        if !rc_core::manifest::is_safe_relative_path(&entry.path) {
            return Err(anyhow!("manifest contains an unsafe path: {}", entry.path));
        }
        wanted.insert(entry.path.clone());
        wanted_entries.insert(entry.path.clone());
        // Directories implied by a file must not be reported as strays.
        let mut cursor = PathBuf::from(&entry.path);
        while let Some(parent) = cursor.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            let rel = parent.to_string_lossy().replace('\\', "/");
            wanted.insert(rel.clone());
            wanted_dirs.insert(rel);
            cursor = parent.to_path_buf();
        }

        if entry.r#type == EntryType::EntrySymlink as i32 {
            // `hash` is blake3 of the target, not the target: using it here
            // builds a link pointing at a 64-hex string (§4.4).
            if !symlink_matches_on_disk(root, entry) {
                plan.symlinks.push((entry.path.clone(), entry.symlink_target.clone()));
            }
            continue;
        }
        if !matches_on_disk(root, entry)? {
            plan.fetch.push(entry.clone());
        }
    }

    for (existing, kind) in walk_relative(root)? {
        if !wanted.contains(&existing) {
            plan.delete.push(existing);
            continue;
        }
        // A directory the manifest implies, but which is a symlink on disk, is
        // how a build escapes the workspace: the next task's `create_dir_all` +
        // write would follow it and land wherever it points — the worker's CAS,
        // for instance. `wanted` alone said "keep", so this used to survive.
        // The same applies to a file entry sitting on top of a real directory,
        // which no write can replace.
        let wrong_kind = (wanted_dirs.contains(&existing) && kind != DiskKind::Dir)
            || (wanted_entries.contains(&existing) && kind == DiskKind::Dir);
        if wrong_kind {
            plan.delete.push(existing);
        }
    }
    plan.delete.sort();
    // Deepest paths first so directories empty out before they are removed.
    plan.delete.reverse();
    Ok(plan)
}

/// A symlink is correct when it *is* a symlink and points where the manifest
/// says. Anything else (missing, a real file, a stale target) needs rewriting.
fn symlink_matches_on_disk(root: &Path, entry: &FileEntry) -> bool {
    let path = root.join(&entry.path);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_symlink() => std::fs::read_link(&path)
            .map(|t| t.to_string_lossy() == entry.symlink_target.as_str())
            .unwrap_or(false),
        _ => false,
    }
}

fn matches_on_disk(root: &Path, entry: &FileEntry) -> Result<bool> {
    let path = root.join(&entry.path);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return Ok(false);
    };
    if meta.is_symlink() || meta.is_dir() {
        return Ok(false);
    }
    if meta.len() != entry.size {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let is_exec = meta.permissions().mode() & 0o111 != 0;
        if is_exec != entry.executable {
            return Ok(false);
        }
    }
    // Size and mode agree; hashing is what actually decides (§4.4).
    Ok(rc_core::cas::hash_file(&path).map(|h| h == entry.hash).unwrap_or(false))
}

/// What a path actually is on disk, by `lstat` — a symlink is a symlink, never
/// whatever it points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskKind {
    Dir,
    Symlink,
    File,
}

fn walk_relative(root: &Path) -> Result<Vec<(String, DiskKind)>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let rel = match path.strip_prefix(root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            // The worker's own bookkeeping is not part of the workspace.
            if rel == ".rc-state.json" {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            // Order matters: a symlink to a directory reports `is_dir()` false
            // here (file_type is lstat-based), but being explicit keeps it that
            // way if this is ever rewritten against metadata().
            let kind = if ft.is_symlink() {
                DiskKind::Symlink
            } else if ft.is_dir() {
                DiskKind::Dir
            } else {
                DiskKind::File
            };
            out.push((rel, kind));
            if kind == DiskKind::Dir {
                stack.push(path);
            }
        }
    }
    Ok(out)
}

/// Remove strays listed in the plan.
pub fn apply_deletions(root: &Path, plan: &RebuildPlan) -> Result<()> {
    for rel in &plan.delete {
        let path = root.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let result = if meta.is_dir() && !meta.is_symlink() {
            // Only prune directories that emptied out; a non-empty one still
            // holds wanted files.
            std::fs::remove_dir(&path).or(Ok(()))
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = result {
            tracing::warn!(path = %path.display(), error = %e, "failed to remove stray path");
        }
    }
    Ok(())
}

/// Materialize `rel` under `root` as a real directory, replacing anything on
/// the way that is not one.
///
/// The previous task's build ran as this uid inside the workspace and could
/// have swapped a directory for a symlink pointing anywhere. Plain
/// `create_dir_all` is happy with that — the path exists and is a directory,
/// through the link — so whatever gets written next lands outside the
/// workspace. Callers that write before the manifest-driven cleanup runs
/// (baseline extraction, notably) have to come through here.
pub fn ensure_real_dir(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    std::fs::create_dir_all(&path)?;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        if part == ".." || part == "." {
            return Err(anyhow!("refusing to traverse `{rel}`"));
        }
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() && !meta.is_symlink() => {}
            Ok(meta) => {
                tracing::warn!(
                    path = %path.display(),
                    symlink = meta.is_symlink(),
                    "replacing a workspace path that should be a directory"
                );
                if meta.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
                std::fs::create_dir(&path)?;
            }
            Err(_) => std::fs::create_dir(&path)?,
        }
    }
    Ok(path)
}

/// Write one file's content with the manifest's mode.
pub fn write_file(root: &Path, entry: &FileEntry, data: &[u8]) -> Result<()> {
    let path = root.join(&entry.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Replace rather than truncate-in-place: the old inode may be hard-linked
    // into a cache.
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, data)?;
    set_mode(&path, entry.executable)?;
    Ok(())
}

pub fn set_mode(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

pub fn write_symlink(root: &Path, rel: &str, target: &str) -> Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&path);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &path)?;
    #[cfg(not(unix))]
    std::fs::write(&path, target)?;
    Ok(())
}

/// Post-condition check: the tree now matches the manifest exactly (§7.3).
pub fn verify(root: &Path, manifest: &Manifest) -> Result<()> {
    let remaining = plan(root, manifest)?;
    if !remaining.fetch.is_empty() {
        return Err(anyhow!(
            "workspace verification failed: {} file(s) still differ, first is {}",
            remaining.fetch.len(),
            remaining.fetch[0].path
        ));
    }
    if !remaining.delete.is_empty() {
        return Err(anyhow!(
            "workspace verification failed: {} stray path(s) remain, first is {}",
            remaining.delete.len(),
            remaining.delete[0]
        ));
    }
    if !remaining.symlinks.is_empty() {
        return Err(anyhow!(
            "workspace verification failed: {} symlink(s) still wrong, first is {}",
            remaining.symlinks.len(),
            remaining.symlinks[0].0
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-{}-{tag}-{}", env!("CARGO_CRATE_NAME"), ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(path: &str, content: &[u8], executable: bool) -> FileEntry {
        FileEntry {
            path: path.into(),
            size: content.len() as u64,
            hash: rc_core::cas::hash_bytes(content),
            r#type: EntryType::EntryFile as i32,
            executable,
            in_baseline: false,
            symlink_target: String::new(),
        }
    }

    fn manifest_of(entries: Vec<FileEntry>) -> Manifest {
        rc_core::manifest::build(entries, "", false)
    }

    #[test]
    fn an_empty_workspace_needs_everything() {
        let root = scratch("empty");
        let m = manifest_of(vec![entry("src/main.rs", b"fn main(){}", false)]);
        let p = plan(&root, &m).unwrap();
        assert_eq!(p.fetch.len(), 1);
        assert!(p.delete.is_empty());
    }

    #[test]
    fn matching_content_is_not_refetched() {
        let root = scratch("match");
        let e = entry("a.rs", b"same", false);
        write_file(&root, &e, b"same").unwrap();
        let p = plan(&root, &manifest_of(vec![e])).unwrap();
        assert!(p.fetch.is_empty());
    }

    #[test]
    fn changed_content_is_refetched_even_at_the_same_size() {
        let root = scratch("changed");
        let old = entry("a.rs", b"aaaa", false);
        write_file(&root, &old, b"aaaa").unwrap();
        let new = entry("a.rs", b"bbbb", false);
        let p = plan(&root, &manifest_of(vec![new])).unwrap();
        assert_eq!(p.fetch.len(), 1, "hash, not size, is the judge (§4.4)");
    }

    #[cfg(unix)]
    #[test]
    fn a_lost_executable_bit_is_detected() {
        // §4.4/§7.3: pre_commands run scripts; losing +x breaks the build in a
        // way that looks like a code problem.
        let root = scratch("mode");
        let plain = entry("run.sh", b"#!/bin/sh\n", false);
        write_file(&root, &plain, b"#!/bin/sh\n").unwrap();
        let exec = entry("run.sh", b"#!/bin/sh\n", true);
        let p = plan(&root, &manifest_of(vec![exec])).unwrap();
        assert_eq!(p.fetch.len(), 1);
    }

    #[test]
    fn files_absent_from_the_manifest_are_scheduled_for_deletion() {
        // §7.3: this is the whole point — a deleted source file must vanish
        // remotely too.
        let root = scratch("delete");
        let keep = entry("keep.rs", b"keep", false);
        let stale = entry("stale.rs", b"stale", false);
        write_file(&root, &keep, b"keep").unwrap();
        write_file(&root, &stale, b"stale").unwrap();
        let p = plan(&root, &manifest_of(vec![keep])).unwrap();
        assert_eq!(p.delete, vec!["stale.rs"]);
    }

    #[test]
    fn deletion_actually_removes_the_file_and_verify_then_passes() {
        let root = scratch("delete2");
        let keep = entry("src/keep.rs", b"keep", false);
        let stale = entry("src/stale.rs", b"stale", false);
        write_file(&root, &keep, b"keep").unwrap();
        write_file(&root, &stale, b"stale").unwrap();
        let m = manifest_of(vec![keep]);
        let p = plan(&root, &m).unwrap();
        apply_deletions(&root, &p).unwrap();
        assert!(!root.join("src/stale.rs").exists());
        assert!(root.join("src/keep.rs").exists(), "the parent dir must survive");
        verify(&root, &m).unwrap();
    }

    #[test]
    fn directories_holding_wanted_files_are_never_pruned() {
        let root = scratch("dirs");
        let deep = entry("a/b/c.rs", b"x", false);
        write_file(&root, &deep, b"x").unwrap();
        let p = plan(&root, &manifest_of(vec![deep])).unwrap();
        assert!(p.delete.is_empty(), "implied parent dirs are wanted");
    }

    /// Built exactly the way the scanner builds one: `hash` is the blake3 of
    /// the target, `symlink_target` is the target itself. The old version of
    /// this fixture put the target string straight into `hash` — a manifest the
    /// scanner can never produce — which is why the bug below survived.
    fn symlink_entry(path: &str, target: &str) -> FileEntry {
        FileEntry {
            path: path.into(),
            size: target.len() as u64,
            hash: rc_core::manifest::symlink_hash(target),
            r#type: EntryType::EntrySymlink as i32,
            executable: false,
            in_baseline: false,
            symlink_target: target.into(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recreated_from_their_target_not_their_hash() {
        // §4.4: never followed; the target text is the content. Reconstructing
        // from `hash` yields a link pointing at 64 hex characters.
        let root = scratch("symlink");
        let m = manifest_of(vec![symlink_entry("link", "../outside/target")]);
        let p = plan(&root, &m).unwrap();
        assert_eq!(p.symlinks, vec![("link".to_string(), "../outside/target".to_string())]);

        write_symlink(&root, &p.symlinks[0].0, &p.symlinks[0].1).unwrap();
        let written = std::fs::read_link(root.join("link")).unwrap();
        assert_eq!(written.to_string_lossy(), "../outside/target");
        assert!(
            !written.to_string_lossy().chars().all(|c| c.is_ascii_hexdigit()),
            "a link named after a hash is the bug this test exists for"
        );
        verify(&root, &m).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_the_wrong_way_is_repaired_and_verify_catches_it() {
        let root = scratch("symlink-stale");
        let m = manifest_of(vec![symlink_entry("link", "new/target")]);
        write_symlink(&root, "link", "stale/target").unwrap();

        assert!(verify(&root, &m).is_err(), "a wrong link must not pass verification");
        let p = plan(&root, &m).unwrap();
        assert_eq!(p.symlinks.len(), 1);
        write_symlink(&root, "link", &p.symlinks[0].1).unwrap();
        verify(&root, &m).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_correct_symlink_is_left_alone() {
        let root = scratch("symlink-stable");
        let m = manifest_of(vec![symlink_entry("link", "real/target")]);
        write_symlink(&root, "link", "real/target").unwrap();
        assert!(plan(&root, &m).unwrap().symlinks.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_for_a_symlink_is_removed_before_anything_is_written() {
        // The build runs as the worker's uid inside the workspace, so it can
        // replace `src/` with a link to somewhere outside. Left in place, the
        // next task's create_dir_all + write follows it and writes there.
        let root = scratch("escape");
        let outside = scratch("outside");
        std::fs::write(outside.join("victim.rs"), b"do not touch").unwrap();

        std::os::unix::fs::symlink(&outside, root.join("src")).unwrap();
        let e = entry("src/main.rs", b"fn main(){}", false);
        let m = manifest_of(vec![e.clone()]);

        let p = plan(&root, &m).unwrap();
        assert!(
            p.delete.contains(&"src".to_string()),
            "the swapped directory must be scheduled for removal, got {:?}",
            p.delete
        );

        // The order below is the fix, and it is the order the runner uses:
        // deletions before writes. Reversed, `write_file` would create_dir_all
        // through the still-present symlink and write outside the workspace,
        // which no later deletion can undo.
        apply_deletions(&root, &p).unwrap();
        write_file(&root, &e, b"fn main(){}").unwrap();

        assert!(!root.join("src").symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(outside.join("victim.rs")).unwrap(),
            "do not touch",
            "nothing may be written outside the workspace"
        );
        assert!(!outside.join("main.rs").exists());
        verify(&root, &m).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writing_before_deleting_is_what_the_escape_needs() {
        // Pins *why* the runner deletes first. If this ever stops escaping,
        // the ordering constraint has moved and the comment above is stale.
        let root = scratch("escape-order");
        let outside = scratch("outside-order");
        std::os::unix::fs::symlink(&outside, root.join("src")).unwrap();

        let e = entry("src/main.rs", b"x", false);
        write_file(&root, &e, b"x").unwrap();
        assert!(
            outside.join("main.rs").exists(),
            "with the swap still in place, the write lands outside — hence deletions first"
        );
    }

    #[test]
    fn a_file_entry_sitting_on_a_directory_is_cleared_out_of_the_way() {
        let root = scratch("filedir");
        std::fs::create_dir_all(root.join("thing")).unwrap();
        let e = entry("thing", b"now a file", false);
        let m = manifest_of(vec![e.clone()]);

        let p = plan(&root, &m).unwrap();
        assert!(p.delete.contains(&"thing".to_string()));
        apply_deletions(&root, &p).unwrap();
        write_file(&root, &e, b"now a file").unwrap();
        verify(&root, &m).unwrap();
    }

    #[test]
    fn a_symlink_without_a_target_is_refused_before_it_reaches_a_worker() {
        // An agent predating `symlink_target`: rebuilding from `hash` would
        // produce a dangling link, so the manifest is rejected instead.
        let mut link = symlink_entry("link", "../elsewhere");
        link.symlink_target = String::new();
        let m = manifest_of(vec![link]);
        let err = rc_core::manifest::validate(&m).unwrap_err();
        assert!(err.contains("no target"), "{err}");
    }

    #[test]
    fn traversal_paths_are_refused_before_any_write() {
        let root = scratch("traversal");
        let mut evil = entry("ok.rs", b"x", false);
        evil.path = "../../etc/passwd".into();
        let m = Manifest {
            entries: vec![evil],
            root_hash: String::new(),
            base_commit: String::new(),
            baseline: false,
            anchor_mount: String::new(),
            roots: Vec::new(),
        };
        assert!(plan(&root, &m).is_err());
    }

    #[test]
    fn verify_fails_loudly_when_content_is_wrong() {
        let root = scratch("verify");
        let e = entry("a.rs", b"expected", false);
        write_file(&root, &entry("a.rs", b"actual!!", false), b"actual!!").unwrap();
        let err = verify(&root, &manifest_of(vec![e])).unwrap_err();
        assert!(err.to_string().contains("still differ"));
    }
}
