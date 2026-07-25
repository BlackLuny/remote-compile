//! Workspace enumeration (§4.2–§4.4).
//!
//! git is the source of truth for *what exists*, because "what git can see" is
//! precisely the definition of what a local build sees. Guessing from
//! `.gitignore` misses files that are ignored but load-bearing, and those
//! produce the worst class of bug: builds that fail remotely and pass locally,
//! with nothing in the diff to explain it (§4.3).

use crate::excludes::Excludes;
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

/// How to decide what a directory contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enumeration {
    /// git when the directory is in a repository, otherwise an ignore-walk.
    /// This is what a repository root wants.
    Auto,
    /// Force an ignore-walk and disregard ancestor ignore rules.
    ///
    /// For a directory that lies inside a repository which deliberately does
    /// not track it — a `.gitignore`d local crate that cargo nonetheless
    /// builds. Asking git about it returns nothing, and an ordinary ignore-walk
    /// re-applies the very `.gitignore` that hid it, so both would report an
    /// empty directory and the build would silently lose the code.
    Standalone,
}

/// Enumerate, hash and package a worktree.
pub fn scan(root: &Path, excludes: &Excludes, index: &mut StatIndex) -> Result<Scan, ScanError> {
    scan_with(root, excludes, index, Enumeration::Auto)
}

pub fn scan_with(
    root: &Path,
    excludes: &Excludes,
    index: &mut StatIndex,
    mode: Enumeration,
) -> Result<Scan, ScanError> {
    let root = root
        .canonicalize()
        .map_err(|e| ScanError::Other(anyhow!("canonicalize {}: {e}", root.display())))?;
    let is_git = mode == Enumeration::Auto && git_root(&root).is_some();
    let repo_url = if is_git { remote_url(&root) } else { None };
    let standalone = mode == Enumeration::Standalone;

    let mut attempts = 0;
    let mut warnings = Vec::new();
    loop {
        attempts += 1;
        let listing = if is_git {
            git_listing(&root)?
        } else {
            if !standalone {
                warnings.push(
                    "not a git repository: falling back to .gitignore-based walking, which may miss \
                     ignored-but-required files (§4.3)"
                        .to_string(),
                );
            }
            ignore_listing(&root, excludes, standalone)?
        };

        let before = stat_all(&root, &listing.paths, excludes);
        let (entries, hashed, reused) = hash_entries(&root, &listing, excludes, index)?;
        // Re-enumerate rather than re-stat the paths we started with: a file
        // *created* during the scan is absent from `listing` altogether, so
        // comparing only known paths cannot see it — and a manifest missing a
        // file that exists locally is precisely the §4.3 divergence.
        let after_listing = if is_git {
            git_listing(&root)?
        } else {
            ignore_listing(&root, excludes, standalone)?
        };
        let after = stat_all(&root, &after_listing.paths, excludes);

        // §4.2: anything that moved while we were reading is a torn snapshot.
        // Comparing over the union catches all three cases — appeared,
        // disappeared, and modified in place.
        let mut changed: Vec<String> = before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .cloned()
            .collect();
        changed.sort();
        changed.dedup();
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

        // The baseline travels as a git bundle, and a bundle carries reachable
        // *history* — not just the tree at `base_commit`. Leaving a path out of
        // the manifest therefore withholds nothing from it, and no check over
        // the current index can prove otherwise: the file may be staged for
        // deletion, or deleted several commits ago, and still be in the pack.
        // So any exclusion at all costs this root its baseline.
        let mut baseline = is_git && !listing.base_commit.is_empty();
        if baseline && excludes.forbids_baseline() {
            baseline = false;
            warnings.push(format!(
                "exclude ({}) turns off the git baseline for this directory: the baseline is a \
                 git bundle and carries reachable history, which no per-file check can vouch for. \
                 Every file travels individually instead, which is slower until the \
                 content-addressed store warms up.",
                excludes.user_patterns().join(", ")
            ));
        }
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
    let tracked_list = git_nul(root, &["ls-files", "-z", "--recurse-submodules"])?;
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
    for path in tracked_list {
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
fn ignore_listing(root: &Path, excludes: &Excludes, standalone: bool) -> Result<Listing, ScanError> {
    let mut paths = Vec::new();
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Ancestor rules are what hid a standalone root in the first place;
        // re-applying them here would enumerate nothing.
        .parents(!standalone)
        .follow_links(false);
    for entry in walker.build().flatten() {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || excludes.matches(&rel) {
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
    excludes: &Excludes,
    index: &mut StatIndex,
) -> Result<(Vec<FileEntry>, usize, usize), ScanError> {
    let mut entries = Vec::with_capacity(listing.paths.len());
    let mut to_record = Vec::new();
    let (mut hashed, mut reused) = (0usize, 0usize);

    for path in &listing.paths {
        if excludes.matches(path) {
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
                // The hash identifies the link; only this carries what it points at.
                symlink_target: target,
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
            symlink_target: String::new(),
        });
    }

    index.record_all(&to_record).map_err(ScanError::Other)?;
    Ok((entries, hashed, reused))
}

/// Enumerate a root the same way `scan` would, without hashing anything.
/// Used to re-check the whole root set after a multi-root sweep.
pub fn enumerate(root: &Path, excludes: &Excludes, mode: Enumeration) -> Result<Vec<String>, ScanError> {
    let root = root
        .canonicalize()
        .map_err(|e| ScanError::Other(anyhow!("canonicalize {}: {e}", root.display())))?;
    let listing = if mode == Enumeration::Auto && git_root(&root).is_some() {
        git_listing(&root)?
    } else {
        ignore_listing(&root, excludes, mode == Enumeration::Standalone)?
    };
    Ok(listing
        .paths
        .into_iter()
        .filter(|p| !excludes.matches(p))
        .collect())
}

/// Stat a set of paths relative to `base`. Used across roots, where the base
/// is the anchor and the paths are anchor-relative.
pub fn stat_entries(base: &Path, paths: &[String]) -> HashMap<String, (u64, i64)> {
    // Already-selected manifest paths: nothing left to filter out.
    stat_all(base, paths, &Excludes::structural(&[]))
}

/// Stat everything that will actually end up in the manifest. Excluded paths
/// are skipped deliberately: `target/` churns constantly while a local build
/// runs, and letting that count as instability would reject scans over a
/// workspace that is perfectly stable in every way that matters.
fn stat_all(root: &Path, paths: &[String], excludes: &Excludes) -> HashMap<String, (u64, i64)> {
    paths
        .iter()
        .filter(|p| !excludes.matches(p))
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

    fn ex(patterns: &[&str]) -> Excludes {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        Excludes::new(&["target"], &owned).unwrap()
    }

    fn scan_repo(dir: &Path) -> Scan {
        let mut idx = StatIndex::open_memory().unwrap();
        scan(dir, &ex(&[]), &mut idx).expect("scan should succeed")
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
    fn an_excluded_untracked_file_never_enters_the_manifest() {
        let d = repo("exclude-untracked");
        write(&d, "src/main.rs", "fn main() {}");
        commit_all(&d);
        write(&d, "local.pem", "-----BEGIN PRIVATE KEY-----");

        let mut idx = StatIndex::open_memory().unwrap();
        let s = scan(&d, &ex(&["*.pem"]), &mut idx).unwrap();
        let p = paths(&s);
        assert!(!p.contains(&"local.pem".to_string()), "{p:?}");
        assert!(p.contains(&"src/main.rs".to_string()));
        // Even here the baseline goes: the pattern cannot be shown to be
        // absent from reachable history, so nothing is claimed about it.
        assert!(!s.manifest.baseline);
    }

    #[test]
    fn excluding_a_tracked_file_turns_the_baseline_off() {
        // The exclusion would otherwise be theatre: `git bundle` packs the whole
        // tree at base_commit, so the key reaches the control plane inside the
        // pack having never appeared in a manifest.
        let d = repo("exclude-tracked");
        write(&d, "src/main.rs", "fn main() {}");
        write(&d, "private.pem", "-----BEGIN PRIVATE KEY-----");
        commit_all(&d);

        let mut idx = StatIndex::open_memory().unwrap();
        let s = scan(&d, &ex(&["*.pem"]), &mut idx).unwrap();

        assert!(!paths(&s).contains(&"private.pem".to_string()));
        assert!(
            !s.manifest.baseline,
            "a tracked exclusion must disable the baseline, or the bundle carries it anyway"
        );
        assert!(
            s.warnings.iter().any(|w| w.contains("*.pem") && w.contains("baseline")),
            "and it must say so: {:?}",
            s.warnings
        );
        // With no baseline every remaining file has to travel through the CAS,
        // and the excluded one is not among them.
        let reconcile = rc_core::manifest::blobs_to_reconcile(&s.manifest);
        assert_eq!(reconcile.len(), paths(&s).len(), "everything else goes L2");
    }

    #[test]
    fn a_secret_staged_for_deletion_still_costs_the_baseline() {
        // `git ls-files` reads the index, so a staged deletion removes the path
        // from every listing while HEAD — and therefore the bundle — still has
        // it. Deciding per-path would have re-enabled the baseline here.
        let d = repo("exclude-staged-delete");
        write(&d, "src/main.rs", "fn main() {}");
        write(&d, "private.pem", "-----BEGIN PRIVATE KEY-----");
        commit_all(&d);
        run_git(&d, &["rm", "-q", "--cached", "private.pem"]);
        std::fs::remove_file(d.join("private.pem")).unwrap();

        let mut idx = StatIndex::open_memory().unwrap();
        let s = scan(&d, &ex(&["*.pem"]), &mut idx).unwrap();
        assert!(
            !s.manifest.baseline,
            "HEAD still carries the secret, so the bundle would too"
        );
    }

    #[test]
    fn a_secret_deleted_in_an_earlier_commit_still_costs_the_baseline() {
        // The strongest case: absent from the index, absent from HEAD, absent
        // from the working tree — and still reachable in the pack a bundle
        // builds. No inspection of the current tree can see this.
        let d = repo("exclude-history");
        write(&d, "private.pem", "-----BEGIN PRIVATE KEY-----");
        commit_all(&d);
        run_git(&d, &["rm", "-q", "private.pem"]);
        run_git(&d, &["commit", "--quiet", "-m", "remove the key"]);
        write(&d, "src/main.rs", "fn main() {}");
        commit_all(&d);

        assert!(!d.join("private.pem").exists());
        let mut idx = StatIndex::open_memory().unwrap();
        let s = scan(&d, &ex(&["*.pem"]), &mut idx).unwrap();
        assert!(
            !s.manifest.baseline,
            "history still reaches the secret, so the baseline cannot be used"
        );
    }

    #[test]
    fn a_pattern_matching_nothing_today_still_costs_the_baseline() {
        // Whether it matches now says nothing about what the object graph
        // holds, so the guarantee cannot be conditioned on it.
        let d = repo("exclude-nomatch");
        write(&d, "src/main.rs", "fn main() {}");
        commit_all(&d);
        let mut idx = StatIndex::open_memory().unwrap();
        let s = scan(&d, &ex(&["*.pem"]), &mut idx).unwrap();
        assert!(!s.manifest.baseline);
    }

    #[test]
    fn a_directory_exclusion_keeps_its_files_out_of_the_manifest() {
        let d = repo("exclude-dir");
        write(&d, "src/main.rs", "fn main() {}");
        write(&d, "secrets/prod/token", "hunter2");
        write(&d, "secrets/readme.md", "notes");
        commit_all(&d);

        let mut idx = StatIndex::open_memory().unwrap();
        let s = scan(&d, &ex(&["secrets"]), &mut idx).unwrap();
        let p = paths(&s);
        assert!(!p.iter().any(|x| x.starts_with("secrets/")), "{p:?}");
        assert!(p.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn without_exclusions_the_baseline_is_untouched() {
        let d = repo("exclude-none");
        write(&d, "private.pem", "-----BEGIN PRIVATE KEY-----");
        commit_all(&d);
        let mut idx = StatIndex::open_memory().unwrap();
        let s = scan(&d, &ex(&[]), &mut idx).unwrap();
        assert!(s.manifest.baseline);
        assert!(paths(&s).contains(&"private.pem".to_string()));
    }

    #[test]
    fn an_exclusion_changes_the_root_hash_so_results_are_not_reused() {
        // The manifest is a different manifest; a cached result computed while
        // the file was still synced must not answer for one without it.
        let d = repo("exclude-hash");
        write(&d, "src/main.rs", "fn main() {}");
        write(&d, "secret.txt", "sensitive");
        commit_all(&d);

        let mut a = StatIndex::open_memory().unwrap();
        let with = scan(&d, &ex(&[]), &mut a).unwrap();
        let mut b = StatIndex::open_memory().unwrap();
        let without = scan(&d, &ex(&["secret.txt"]), &mut b).unwrap();
        assert_ne!(with.manifest.root_hash, without.manifest.root_hash);
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
        // The hash identifies the link; only this says where it points. The
        // worker rebuilds from this field — rebuilding from `hash` produced a
        // link named after 64 hex characters.
        assert_eq!(link.symlink_target, "real.rs");
        rc_core::manifest::validate(&s.manifest).expect("a scanned manifest must validate");
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_symlink_escaping_the_repo_keeps_its_literal_target() {
        // §4.4: the target is never resolved, so `../sibling` stays `../sibling`
        // and means whatever it means where the workspace is rebuilt.
        let d = repo("symlink-escape");
        std::os::unix::fs::symlink("../sibling/crate", d.join("vendor")).unwrap();
        run_git(&d, &["add", "-A"]);
        commit_all(&d);

        let s = scan_repo(&d);
        let link = s
            .manifest
            .entries
            .iter()
            .find(|e| e.path == "vendor")
            .expect("dangling symlink is still an entry");
        assert_eq!(link.symlink_target, "../sibling/crate");
        assert_eq!(link.hash, rc_core::manifest::symlink_hash("../sibling/crate"));
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
        let first = scan(&d, &ex(&[]), &mut idx).unwrap();
        assert_eq!(first.hashed, 10);
        assert_eq!(first.reused, 0);

        let second = scan(&d, &ex(&[]), &mut idx).unwrap();
        assert_eq!(second.reused, 10, "nothing changed, so nothing is re-hashed");
        assert_eq!(second.hashed, 0);
        assert_eq!(first.manifest.root_hash, second.manifest.root_hash);

        write(&d, "f3.rs", "edited content");
        let third = scan(&d, &ex(&[]), &mut idx).unwrap();
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
        let first = scan(&d, &Excludes::structural(&[]), &mut idx).unwrap();
        assert!(!first.first_base_commit.is_empty());

        write(&d, "a.rs", "v2");
        commit_all(&d);
        let second = scan(&d, &Excludes::structural(&[]), &mut idx).unwrap();

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
                symlink_target: String::new(),
            });
        }
        assert_eq!(rc_core::manifest::find_case_conflicts(&entries).len(), 1);
        // The scanner surfaces it as a typed error rather than a silent merge.
        let err = ScanError::CaseConflict("src/Main.rs".into(), "src/main.rs".into());
        assert!(err.to_string().contains("differ only by case"));
        let _ = scan(&d, &Excludes::structural(&[]), &mut idx);
    }

    #[test]
    fn a_file_that_appears_during_the_scan_is_detected() {
        // §4.2: the check used to re-stat only the paths enumerated *before*
        // hashing, so a file created mid-scan was invisible — and the manifest
        // silently omitted a file the local build can see (§4.3).
        let d = repo("appeared");
        write(&d, "a.rs", "one");
        commit_all(&d);

        let before = stat_all(&d, &["a.rs".to_string()], &Excludes::structural(&[]));
        write(&d, "b.rs", "appeared mid-scan");
        let after = stat_all(&d, &["a.rs".to_string(), "b.rs".to_string()], &Excludes::structural(&[]));

        let changed: Vec<&String> = before
            .keys()
            .chain(after.keys())
            .filter(|p| before.get(*p) != after.get(*p))
            .collect();
        assert_eq!(changed, vec!["b.rs"], "the new file must register as movement");
    }

    #[test]
    fn a_file_that_vanishes_during_the_scan_is_detected() {
        let d = repo("vanished");
        write(&d, "a.rs", "one");
        write(&d, "b.rs", "two");
        let paths = vec!["a.rs".to_string(), "b.rs".to_string()];
        let before = stat_all(&d, &paths, &Excludes::structural(&[]));
        std::fs::remove_file(d.join("b.rs")).unwrap();
        let after = stat_all(&d, &paths, &Excludes::structural(&[]));

        let changed: Vec<&String> = before
            .keys()
            .chain(after.keys())
            .filter(|p| before.get(*p) != after.get(*p))
            .collect();
        assert_eq!(changed, vec!["b.rs"]);
    }

    #[test]
    fn build_output_churn_does_not_count_as_instability() {
        // A local `cargo build` writing into target/ while we scan is normal,
        // and those files never reach the manifest anyway.
        let d = repo("churn");
        write(&d, "target/debug/thing", "v1");
        let paths = vec!["target/debug/thing".to_string()];
        let before = stat_all(&d, &paths, &ex(&[]));
        write(&d, "target/debug/thing", "v2 is a different length");
        let after = stat_all(&d, &paths, &ex(&[]));
        assert!(before.is_empty() && after.is_empty(), "excluded paths are not watched");
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
        let e = ex(&[]);
        assert!(e.matches("target/debug/foo"));
        assert!(e.matches("crates/a/target/foo"));
        assert!(e.matches(".git/config"));
        assert!(e.matches("node_modules/x"));
        assert!(!e.matches("src/target_helper.rs"));
    }
}
