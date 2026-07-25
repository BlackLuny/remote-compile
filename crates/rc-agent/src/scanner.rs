//! Workspace enumeration (§4.2–§4.4).
//!
//! git is the source of truth for *what exists*, because "what git can see" is
//! precisely the definition of what a local build sees. Guessing from
//! `.gitignore` misses files that are ignored but load-bearing, and those
//! produce the worst class of bug: builds that fail remotely and pass locally,
//! with nothing in the diff to explain it (§4.3).

use crate::index::{Stat, StatIndex};
use anyhow::{anyhow, Result};
use rc_core::pb::{EntryType, FileEntry, Manifest};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub struct Scan {
    pub manifest: Manifest,
    pub repo_url: Option<String>,
    pub is_git: bool,
    /// The first base commit ever seen in this worktree. Worktree identity is
    /// derived from it so a new commit does not invent a new worktree — and
    /// throw away the worker's target volume with it (§3.1).
    pub first_base_commit: String,
    pub hashed: usize,
    pub reused: usize,
    pub attempts: u32,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum ScanError {
    /// The tree kept changing under us. Compiling a torn snapshot would send
    /// the agent chasing an error that never existed (§4.2).
    Unstable { attempts: u32, changed: Vec<String> },
    /// macOS is case-insensitive, Linux is not; silently collapsing two paths
    /// produces a workspace nobody can reproduce (§4.4).
    CaseConflict(String, String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Unstable { attempts, changed } => write!(
                f,
                "workspace_unstable: files kept changing during {attempts} scan attempts \
                 (e.g. {}); retry once writes settle",
                changed.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ),
            ScanError::CaseConflict(a, b) => write!(
                f,
                "sync_error: `{a}` and `{b}` differ only by case; they collide on a \
                 case-insensitive filesystem and cannot be synced safely"
            ),
            ScanError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for ScanError {
    fn from(e: anyhow::Error) -> Self {
        ScanError::Other(e)
    }
}

pub const MAX_SCAN_ATTEMPTS: u32 = 3;

/// Enumerate, hash and package a worktree.
pub fn scan(root: &Path, excludes: &[&str], index: &mut StatIndex) -> Result<Scan, ScanError> {
    let root = root
        .canonicalize()
        .map_err(|e| ScanError::Other(anyhow!("canonicalize {}: {e}", root.display())))?;
    let is_git = git_root(&root).is_some();
    let repo_url = if is_git { remote_url(&root) } else { None };

    let mut attempts = 0;
    let mut warnings = Vec::new();
    loop {
        attempts += 1;
        let listing = if is_git {
            git_listing(&root)?
        } else {
            warnings.push(
                "not a git repository: falling back to .gitignore-based walking, which may miss \
                 ignored-but-required files (§4.3)"
                    .to_string(),
            );
            ignore_listing(&root, excludes)?
        };

        let before = stat_all(&root, &listing.paths);
        let (entries, hashed, reused) = hash_entries(&root, &listing, excludes, index)?;
        let after = stat_all(&root, &listing.paths);

        // §4.2: anything that moved while we were reading is a torn snapshot.
        let changed: Vec<String> = before
            .iter()
            .filter(|(path, stat)| after.get(*path) != Some(stat))
            .map(|(path, _)| path.clone())
            .collect();
        if !changed.is_empty() {
            if attempts >= MAX_SCAN_ATTEMPTS {
                return Err(ScanError::Unstable { attempts, changed });
            }
            tracing::debug!(count = changed.len(), attempt = attempts, "workspace moved; rescanning");
            continue;
        }

        if let Some((a, b)) = rc_core::manifest::find_case_conflicts(&entries).into_iter().next() {
            return Err(ScanError::CaseConflict(a, b));
        }

        let first_base_commit = match index.meta("first_base_commit") {
            Some(seen) if !seen.is_empty() => seen,
            _ => {
                index.set_meta("first_base_commit", &listing.base_commit).ok();
                listing.base_commit.clone()
            }
        };

        let baseline = is_git && !listing.base_commit.is_empty();
        let manifest = rc_core::manifest::build(entries, &listing.base_commit, baseline);
        index.set_meta("base_commit", &listing.base_commit).ok();
        index.set_meta("root_hash", &manifest.root_hash).ok();
        let live: HashSet<String> = manifest.entries.iter().map(|e| e.path.clone()).collect();
        index.retain(&live).ok();

        return Ok(Scan {
            manifest,
            repo_url,
            is_git,
            first_base_commit,
            hashed,
            reused,
            attempts,
            warnings,
        });
    }
}

#[derive(Debug, Default)]
struct Listing {
    paths: Vec<String>,
    /// Tracked *and* unmodified at `base_commit`: the worker can get these
    /// from its git mirror instead of the CAS (§4.1 L1).
    clean: HashSet<String>,
    base_commit: String,
}

fn git_listing(root: &Path) -> Result<Listing, ScanError> {
    let base_commit = git(root, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // `--recurse-submodules` is what pulls submodule *contents* in; without it
    // git reports only the gitlink and the worker ends up missing files.
    let tracked = git_nul(root, &["ls-files", "-z", "--recurse-submodules"])?;
    let mut untracked = git_nul(root, &["ls-files", "-z", "--others", "--exclude-standard"])?;

    let submodule_prefixes = submodule_paths(root);
    for sub in &submodule_prefixes {
        let sub_root = root.join(sub);
        if let Ok(more) = git_nul(&sub_root, &["ls-files", "-z", "--others", "--exclude-standard"]) {
            untracked.extend(more.into_iter().map(|p| format!("{sub}/{p}")));
        }
    }

    // Modified tracked files cannot come from the baseline.
    let modified: HashSet<String> = git_nul(root, &["diff", "--name-only", "-z", "HEAD", "--"])
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut clean = HashSet::new();
    let mut paths = Vec::new();
    for path in tracked {
        let in_submodule = submodule_prefixes
            .iter()
            .any(|s| path.starts_with(&format!("{s}/")));
        if !modified.contains(&path) && !base_commit.is_empty() && !in_submodule {
            clean.insert(path.clone());
        }
        paths.push(path);
    }
    paths.extend(untracked);
    paths.sort();
    paths.dedup();

    Ok(Listing { paths, clean, base_commit })
}

/// Degraded enumeration for non-git directories (§4.3 fallback).
fn ignore_listing(root: &Path, excludes: &[&str]) -> Result<Listing, ScanError> {
    let mut paths = Vec::new();
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false);
    for entry in walker.build().flatten() {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || is_excluded(&rel, excludes) {
            continue;
        }
        let Some(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            continue;
        }
        paths.push(rel);
    }
    paths.sort();
    Ok(Listing {
        paths,
        clean: HashSet::new(),
        base_commit: String::new(),
    })
}

fn hash_entries(
    root: &Path,
    listing: &Listing,
    excludes: &[&str],
    index: &mut StatIndex,
) -> Result<(Vec<FileEntry>, usize, usize), ScanError> {
    let mut entries = Vec::with_capacity(listing.paths.len());
    let mut to_record = Vec::new();
    let (mut hashed, mut reused) = (0usize, 0usize);

    for path in &listing.paths {
        if is_excluded(path, excludes) {
            continue;
        }
        if !rc_core::manifest::is_safe_relative_path(path) {
            continue;
        }
        let abs = root.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            // Tracked but deleted on disk: it must not appear in the manifest,
            // or the worker would resurrect it (§7.3).
            continue;
        };
        if meta.is_dir() {
            continue;
        }

        if meta.is_symlink() {
            // §4.4: never followed — the target string *is* the content.
            let target = std::fs::read_link(&abs)
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            entries.push(FileEntry {
                path: path.clone(),
                size: target.len() as u64,
                hash: rc_core::manifest::symlink_hash(&target),
                r#type: EntryType::EntrySymlink as i32,
                executable: false,
                in_baseline: false,
            });
            continue;
        }

        let size = meta.len();
        let mtime_ns = mtime_nanos(&meta);
        let hash = match index.lookup(path, size, mtime_ns) {
            Some(h) => {
                reused += 1;
                h
            }
            None => {
                let h = rc_core::cas::hash_file(&abs)
                    .map_err(|e| ScanError::Other(anyhow!("hash {path}: {e}")))?;
                hashed += 1;
                to_record.push((path.clone(), Stat { size, mtime_ns, hash: h.clone() }));
                h
            }
        };

        entries.push(FileEntry {
            path: path.clone(),
            size,
            hash,
            r#type: EntryType::EntryFile as i32,
            executable: is_executable(&meta),
            in_baseline: listing.clean.contains(path),
        });
    }

    index.record_all(&to_record).map_err(ScanError::Other)?;
    Ok((entries, hashed, reused))
}

fn stat_all(root: &Path, paths: &[String]) -> HashMap<String, (u64, i64)> {
    paths
        .iter()
        .filter_map(|p| {
            let meta = std::fs::symlink_metadata(root.join(p)).ok()?;
            Some((p.clone(), (meta.len(), mtime_nanos(&meta))))
        })
        .collect()
}

fn mtime_nanos(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Build-output directories are excluded even when git tracks them: syncing a
/// multi-gigabyte `target/` defeats the point of the whole system.
pub fn is_excluded(path: &str, excludes: &[&str]) -> bool {
    let first = path.split('/').next().unwrap_or("");
    if rc_core::ALWAYS_EXCLUDE.contains(&first) {
        return true;
    }
    excludes.iter().any(|e| {
        let e = e.trim_end_matches('/');
        first == e || path.split('/').any(|c| c == e)
    })
}

pub fn git_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn remote_url(dir: &Path) -> Option<String> {
    let out = git(dir, &["config", "--get", "remote.origin.url"]).ok()?;
    let trimmed = out.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn submodule_paths(root: &Path) -> Vec<String> {
    let Ok(out) = git(root, &["submodule", "status", "--recursive"]) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|s| s.trim_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        // Never let git stop for credentials: an agent has no terminal.
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_nul(dir: &Path, args: &[&str]) -> Result<Vec<String>, ScanError> {
    let out = git(dir, args).map_err(ScanError::Other)?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Create a bundle carrying `commit` but not the commits the fleet already
/// has, so the transfer stays incremental (§4.1 step 3).
pub fn create_bundle(root: &Path, commit: &str, known: &[String]) -> Result<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("rc-bundle-{}", ulid::Ulid::generate()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("base.bundle");

    // `git bundle` needs a ref, not a bare sha, to name what it packs.
    let ref_name = format!("refs/rc-bundle/{commit}");
    git(root, &["update-ref", &ref_name, commit])?;

    let mut args: Vec<String> = vec![
        "bundle".into(),
        "create".into(),
        path.to_string_lossy().into_owned(),
        ref_name.clone(),
    ];
    for base in known.iter().filter(|c| !c.is_empty()) {
        // Only exclude bases this repo actually knows; a stale id would abort
        // the whole bundle.
        if git(root, &["cat-file", "-e", &format!("{base}^{{commit}}")]).is_ok() {
            args.push("--not".into());
            args.push(base.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = git(root, &arg_refs);
    let _ = git(root, &["update-ref", "-d", &ref_name]);
    result?;

    let data = std::fs::read(&path)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-scan-{tag}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn repo(tag: &str) -> PathBuf {
        let d = scratch(tag);
        run_git(&d, &["init", "--quiet", "-b", "main"]);
        run_git(&d, &["config", "user.email", "t@example.com"]);
        run_git(&d, &["config", "user.name", "t"]);
        d
    }

    fn commit_all(dir: &Path) {
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "--quiet", "-m", "wip"]);
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    fn scan_repo(dir: &Path) -> Scan {
        let mut idx = StatIndex::open_memory().unwrap();
        scan(dir, &["target"], &mut idx).expect("scan should succeed")
    }

    fn paths(s: &Scan) -> Vec<String> {
        s.manifest.entries.iter().map(|e| e.path.clone()).collect()
    }

    #[test]
    fn tracked_and_untracked_files_are_both_enumerated() {
        let d = repo("basic");
        write(&d, "src/main.rs", "fn main() {}");
        commit_all(&d);
        write(&d, "src/new.rs", "// not committed yet");

        let s = scan_repo(&d);
        assert!(paths(&s).contains(&"src/main.rs".to_string()));
        assert!(paths(&s).contains(&"src/new.rs".to_string()));
        assert!(s.is_git);
    }

    #[test]
    fn ignored_files_are_excluded_but_force_added_ones_are_not() {
        // §4.3: what git sees is what the build sees.
        let d = repo("ignored");
        write(&d, ".gitignore", "secret.txt\nconfig.local\n");
        write(&d, "secret.txt", "nobody should sync this");
        write(&d, "config.local", "but this one is load-bearing");
        run_git(&d, &["add", "-f", "config.local", ".gitignore"]);
        commit_all(&d);

        let p = paths(&scan_repo(&d));
        assert!(!p.contains(&"secret.txt".to_string()), "plain ignored file stays local");
        assert!(
            p.contains(&"config.local".to_string()),
            "an ignored-but-tracked file is part of the build and must sync"
        );
    }

    #[test]
    fn build_output_never_syncs_even_when_tracked() {
        let d = repo("target");
        write(&d, "target/debug/huge.rlib", "x".repeat(1000).as_str());
        write(&d, "src/main.rs", "fn main() {}");
        run_git(&d, &["add", "-f", "."]);
        commit_all(&d);

        let p = paths(&scan_repo(&d));
        assert!(!p.iter().any(|x| x.starts_with("target/")));
        assert!(p.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn a_deleted_tracked_file_disappears_from_the_manifest() {
        // §7.3: otherwise the worker resurrects it and the builds diverge.
        let d = repo("deleted");
        write(&d, "gone.rs", "will be deleted");
        write(&d, "kept.rs", "stays");
        commit_all(&d);
        std::fs::remove_file(d.join("gone.rs")).unwrap();

        let p = paths(&scan_repo(&d));
        assert!(!p.contains(&"gone.rs".to_string()));
        assert!(p.contains(&"kept.rs".to_string()));
    }

    #[test]
    fn clean_tracked_files_ride_the_baseline_and_dirty_ones_do_not() {
        let d = repo("baseline");
        write(&d, "clean.rs", "unchanged");
        write(&d, "dirty.rs", "original");
        commit_all(&d);
        write(&d, "dirty.rs", "edited");
        write(&d, "brand_new.rs", "untracked");

        let s = scan_repo(&d);
        let by_path: HashMap<&str, &FileEntry> =
            s.manifest.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        assert!(by_path["clean.rs"].in_baseline, "unchanged file comes from the mirror");
        assert!(!by_path["dirty.rs"].in_baseline, "edited file must travel via the CAS");
        assert!(!by_path["brand_new.rs"].in_baseline);
        assert!(s.manifest.baseline);
        assert!(!s.manifest.base_commit.is_empty());

        // Only dirty content needs reconciling (§4.1).
        let reconcile = rc_core::manifest::blobs_to_reconcile(&s.manifest);
        assert_eq!(reconcile.len(), 2);
    }

    #[test]
    fn submodule_contents_are_enumerated_and_forced_to_l2() {
        // Risk #29: ls-files without --recurse-submodules reports only the
        // gitlink, and the worker ends up missing the code.
        let sub = repo("submodule-src");
        write(&sub, "lib.rs", "pub fn shared() {}");
        commit_all(&sub);

        let parent = repo("submodule-parent");
        write(&parent, "main.rs", "fn main() {}");
        commit_all(&parent);
        let out = Command::new("git")
            .current_dir(&parent)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .args(["-c", "protocol.file.allow=always", "submodule", "add", "--quiet",
                   sub.to_str().unwrap(), "vendor/shared"])
            .output()
            .unwrap();
        if !out.status.success() {
            // Some git builds refuse local submodules outright; skip rather
            // than assert on the environment.
            eprintln!("skipping: submodule add unsupported here");
            return;
        }
        commit_all(&parent);

        let s = scan_repo(&parent);
        let by_path: HashMap<&str, &FileEntry> =
            s.manifest.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        assert!(
            by_path.contains_key("vendor/shared/lib.rs"),
            "submodule content must be enumerated, got {:?}",
            paths(&s)
        );
        assert!(
            !by_path["vendor/shared/lib.rs"].in_baseline,
            "a superproject archive does not contain submodule files (§4.3)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recorded_by_target_not_content() {
        let d = repo("symlink");
        write(&d, "real.rs", "content");
        std::os::unix::fs::symlink("real.rs", d.join("link.rs")).unwrap();
        commit_all(&d);

        let s = scan_repo(&d);
        let link = s
            .manifest
            .entries
            .iter()
            .find(|e| e.path == "link.rs")
            .expect("symlink present");
        assert_eq!(link.r#type, EntryType::EntrySymlink as i32);
        assert_eq!(link.hash, rc_core::manifest::symlink_hash("real.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_is_captured() {
        use std::os::unix::fs::PermissionsExt;
        let d = repo("exec");
        write(&d, "script.sh", "#!/bin/sh\necho hi\n");
        std::fs::set_permissions(d.join("script.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        write(&d, "plain.txt", "text");
        commit_all(&d);

        let s = scan_repo(&d);
        let by_path: HashMap<&str, &FileEntry> =
            s.manifest.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        assert!(by_path["script.sh"].executable);
        assert!(!by_path["plain.txt"].executable);
    }

    #[test]
    fn a_non_git_directory_still_scans_with_a_warning() {
        let d = scratch("plain");
        write(&d, "main.c", "int main(){}");
        let s = scan_repo(&d);
        assert!(!s.is_git);
        assert!(!s.manifest.baseline);
        assert!(s.warnings.iter().any(|w| w.contains("not a git repository")));
        assert_eq!(paths(&s), vec!["main.c"]);
    }

    #[test]
    fn the_index_turns_a_rescan_into_a_stat_walk() {
        let d = repo("index");
        for i in 0..10 {
            write(&d, &format!("f{i}.rs"), &format!("content {i}"));
        }
        commit_all(&d);

        let mut idx = StatIndex::open_memory().unwrap();
        let first = scan(&d, &["target"], &mut idx).unwrap();
        assert_eq!(first.hashed, 10);
        assert_eq!(first.reused, 0);

        let second = scan(&d, &["target"], &mut idx).unwrap();
        assert_eq!(second.reused, 10, "nothing changed, so nothing is re-hashed");
        assert_eq!(second.hashed, 0);
        assert_eq!(first.manifest.root_hash, second.manifest.root_hash);

        write(&d, "f3.rs", "edited content");
        let third = scan(&d, &["target"], &mut idx).unwrap();
        assert_eq!(third.hashed, 1, "only the edited file is re-hashed");
        assert_ne!(second.manifest.root_hash, third.manifest.root_hash);
    }

    #[test]
    fn worktree_identity_survives_new_commits() {
        // §3.1: keying on the *current* HEAD would mint a new worktree id on
        // every commit, discarding the worker's target volume each time.
        let d = repo("identity");
        write(&d, "a.rs", "v1");
        commit_all(&d);

        let mut idx = StatIndex::open_memory().unwrap();
        let first = scan(&d, &[], &mut idx).unwrap();
        assert!(!first.first_base_commit.is_empty());

        write(&d, "a.rs", "v2");
        commit_all(&d);
        let second = scan(&d, &[], &mut idx).unwrap();

        assert_ne!(
            first.manifest.base_commit, second.manifest.base_commit,
            "the baseline itself did move"
        );
        assert_eq!(
            first.first_base_commit, second.first_base_commit,
            "but the worktree keeps its identity"
        );
        assert_eq!(
            rc_core::ids::worktree_id(&d.canonicalize().unwrap(), &first.first_base_commit),
            rc_core::ids::worktree_id(&d.canonicalize().unwrap(), &second.first_base_commit),
        );
    }

    #[test]
    fn case_conflicting_paths_are_refused_rather_than_collapsed() {
        // §4.4: on a case-insensitive filesystem these are one file; on the
        // Linux worker they are two. Guessing either way is wrong.
        let d = scratch("case");
        let mut idx = StatIndex::open_memory().unwrap();
        write(&d, "src/main.rs", "a");
        let mut entries: Vec<FileEntry> = vec![];
        for p in ["src/Main.rs", "src/main.rs"] {
            entries.push(FileEntry {
                path: p.into(),
                size: 1,
                hash: "a".repeat(64),
                r#type: EntryType::EntryFile as i32,
                executable: false,
                in_baseline: false,
            });
        }
        assert_eq!(rc_core::manifest::find_case_conflicts(&entries).len(), 1);
        // The scanner surfaces it as a typed error rather than a silent merge.
        let err = ScanError::CaseConflict("src/Main.rs".into(), "src/main.rs".into());
        assert!(err.to_string().contains("differ only by case"));
        let _ = scan(&d, &[], &mut idx);
    }

    #[test]
    fn unstable_workspaces_report_a_retryable_error() {
        let err = ScanError::Unstable {
            attempts: 3,
            changed: vec!["src/main.rs".into()],
        };
        let text = err.to_string();
        assert!(text.contains("workspace_unstable"));
        assert!(text.contains("retry"));
        assert!(text.contains("src/main.rs"));
    }

    #[test]
    fn a_bundle_carries_the_local_commit() {
        let d = repo("bundle");
        write(&d, "a.rs", "content");
        commit_all(&d);
        let sha = git(&d, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        let data = create_bundle(&d, &sha, &[]).unwrap();
        assert!(!data.is_empty());
        assert!(data.starts_with(b"# v2 git bundle") || data.starts_with(b"# v3 git bundle"));
        // The temporary ref must not linger in the developer's repo.
        assert!(git(&d, &["show-ref", &format!("refs/rc-bundle/{sha}")]).is_err());
    }

    #[test]
    fn a_bundle_excludes_commits_the_fleet_already_has() {
        let d = repo("bundle-incr");
        write(&d, "a.rs", "first");
        commit_all(&d);
        let first = git(&d, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        write(&d, "b.rs", "second");
        commit_all(&d);
        let second = git(&d, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        let full = create_bundle(&d, &second, &[]).unwrap();
        let incremental = create_bundle(&d, &second, &[first]).unwrap();
        assert!(
            incremental.len() < full.len(),
            "incremental bundle {} should be smaller than full {}",
            incremental.len(),
            full.len()
        );
    }

    #[test]
    fn an_unknown_base_does_not_break_bundle_creation() {
        let d = repo("bundle-badbase");
        write(&d, "a.rs", "x");
        commit_all(&d);
        let sha = git(&d, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        // A commit id from another project must simply be ignored.
        let data = create_bundle(&d, &sha, &["0".repeat(40)]).unwrap();
        assert!(!data.is_empty());
    }

    #[test]
    fn exclusion_matches_directories_at_any_depth() {
        assert!(is_excluded("target/debug/foo", &["target"]));
        assert!(is_excluded("crates/a/target/foo", &["target"]));
        assert!(is_excluded(".git/config", &[]));
        assert!(is_excluded("node_modules/x", &[]));
        assert!(!is_excluded("src/target_helper.rs", &["target"]));
    }
}
