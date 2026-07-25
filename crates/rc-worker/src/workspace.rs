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

    for entry in &manifest.entries {
        if !rc_core::manifest::is_safe_relative_path(&entry.path) {
            return Err(anyhow!("manifest contains an unsafe path: {}", entry.path));
        }
        wanted.insert(entry.path.clone());
        // Directories implied by a file must not be reported as strays.
        let mut cursor = PathBuf::from(&entry.path);
        while let Some(parent) = cursor.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            wanted.insert(parent.to_string_lossy().replace('\\', "/"));
            cursor = parent.to_path_buf();
        }

        if entry.r#type == EntryType::EntrySymlink as i32 {
            plan.symlinks.push((entry.path.clone(), entry.hash.clone()));
            continue;
        }
        if !matches_on_disk(root, entry)? {
            plan.fetch.push(entry.clone());
        }
    }

    for existing in walk_relative(root)? {
        if !wanted.contains(&existing) {
            plan.delete.push(existing);
        }
    }
    plan.delete.sort();
    // Deepest paths first so directories empty out before they are removed.
    plan.delete.reverse();
    Ok(plan)
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

fn walk_relative(root: &Path) -> Result<Vec<String>> {
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
            out.push(rel);
            if ft.is_dir() && !ft.is_symlink() {
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

/// Create `dir` and every component of it below `root`, writable by the build
/// container.
///
/// The sandbox drops all capabilities (§7.1), so the container's root does not
/// get the usual root exemption from file permissions. These directories belong
/// to the worker's own user, which would leave `cargo` unable to create so much
/// as a `Cargo.lock`. Opening them costs nothing: the contents came from the
/// submitting agent to begin with, and the whole tree is throwaway.
pub fn ensure_writable_dir(root: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o777);
        let mut cur = dir;
        loop {
            std::fs::set_permissions(cur, perms.clone())?;
            if cur == root {
                break;
            }
            match cur.parent() {
                Some(p) if p.starts_with(root) => cur = p,
                _ => break,
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

/// Open up every directory in the materialised workspace, for the reason in
/// [`ensure_writable_dir`].
///
/// `write_file` covers the directories the L2 layer creates, but the L1 git
/// baseline is extracted wholesale and arrives with git's own modes — so a
/// `pre_command` or a build script writing anywhere but the root would still
/// hit EACCES without this pass.
pub fn make_tree_writable(root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o777);
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            std::fs::set_permissions(&dir, perms.clone())?;
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                // file_type() does not follow symlinks, so a link pointing out
                // of the tree cannot drag us along with it.
                if entry.file_type()?.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

/// Write one file's content with the manifest's mode.
pub fn write_file(root: &Path, entry: &FileEntry, data: &[u8]) -> Result<()> {
    let path = root.join(&entry.path);
    if let Some(parent) = path.parent() {
        ensure_writable_dir(root, parent)?;
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
        ensure_writable_dir(root, parent)?;
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

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recreated_from_their_target_string() {
        // §4.4: never followed; the target text is the content.
        let root = scratch("symlink");
        let mut link = entry("link", b"", false);
        link.r#type = EntryType::EntrySymlink as i32;
        link.hash = "../outside/target".into();
        let m = manifest_of(vec![link]);
        let p = plan(&root, &m).unwrap();
        assert_eq!(p.symlinks, vec![("link".to_string(), "../outside/target".to_string())]);
        write_symlink(&root, "link", "../outside/target").unwrap();
        assert_eq!(
            std::fs::read_link(root.join("link")).unwrap().to_string_lossy(),
            "../outside/target"
        );
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
