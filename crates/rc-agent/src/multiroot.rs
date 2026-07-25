//! Scanning several local directories into one manifest.
//!
//! The layout rules live in `rc_core::roots`; this is the part that actually
//! reads the disk. Each root is scanned by the ordinary single-root scanner —
//! its own `git ls-files`, its own `.gitignore`, its own stat index — and the
//! resulting paths are prefixed with the root's mount so one flat entry list
//! describes the whole tree.
//!
//! Only the primary root uses the L1 git baseline. Extra roots go entirely
//! through the content-addressed layer, which is the same call §4.3 already
//! makes for submodules: CAS deduplication means the cost is one full upload
//! the first time and nothing after that, and it avoids having to reason about
//! several git mirrors materialising into one tree.

use crate::excludes::Excludes;
use crate::index::StatIndex;
use crate::scanner::{self, Scan, ScanError};
use rc_core::pb::{FileEntry, Manifest, RootInfo};
use rc_core::roots::{self, Layout, RootMount};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The file whose presence in the enclosing enumeration decides whether a
/// nested root is already covered.
const MANIFEST_FILE: &str = "Cargo.toml";

pub struct MultiScan {
    pub manifest: Manifest,
    /// Directory the manifest's paths are relative to. Upload reads files from
    /// here, not from the primary root.
    pub anchor: PathBuf,
    pub repo_url: Option<String>,
    pub is_git: bool,
    pub first_base_commit: String,
    pub warnings: Vec<String>,
    /// Roots actually scanned, in mount order.
    pub scanned: Vec<RootMount>,
    pub hashed: usize,
    pub reused: usize,
}

/// Scan every root in `layout` and merge the results.
///
/// `open_index` yields the per-root stat index, so callers control where those
/// live. Roots nested inside another root are skipped when the enclosing root's
/// enumeration already covers them — and scanned when it does not, which is
/// what happens to a path dependency sitting in a `.gitignore`d directory.
pub fn scan_all(
    layout: &Layout,
    excludes: &Excludes,
    mut open_index: impl FnMut(&Path) -> anyhow::Result<StatIndex>,
) -> Result<MultiScan, ScanError> {
    for attempt in 1..=scanner::MAX_SCAN_ATTEMPTS {
        let outcome = scan_once(layout, excludes, &mut open_index)?;
        let Some(changed) = outcome.unstable else {
            return Ok(outcome.scan);
        };
        if attempt == scanner::MAX_SCAN_ATTEMPTS {
            return Err(ScanError::Unstable { attempts: attempt, changed });
        }
        tracing::debug!(
            count = changed.len(),
            attempt,
            "a root moved while another was being scanned; rescanning all of them"
        );
    }
    unreachable!("the loop returns on its last iteration")
}

struct Once {
    scan: MultiScan,
    /// Paths that moved between the start and end of the whole sweep.
    unstable: Option<Vec<String>>,
}

fn scan_once(
    layout: &Layout,
    excludes: &Excludes,
    open_index: &mut impl FnMut(&Path) -> anyhow::Result<StatIndex>,
) -> Result<Once, ScanError> {
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut roots_info: Vec<RootInfo> = Vec::new();
    let mut scanned: Vec<RootMount> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut covered: HashSet<String> = HashSet::new();
    let mut primary: Option<Scan> = None;
    let (mut hashed, mut reused) = (0usize, 0usize);
    // Stats as each root finished, so a root that changes while a *later* root
    // is being scanned is still caught (§4.2 across roots, not just within one).
    let mut seen_stats: HashMap<String, (u64, i64)> = HashMap::new();

    for (index, root) in layout.roots.iter().enumerate() {
        if root.nested {
            // "Some file under this path was enumerated" is too weak: a repo
            // that tracks `vendor/foo/README` while ignoring the crate itself
            // would look covered. The manifest is the file that decides whether
            // the enclosing repository knows about this crate at all — if git
            // lists it, the crate is a tracked part of the repo and anything
            // ignored inside it was ignored on purpose (§4.3).
            let manifest_path = roots::anchored(&root.mount, MANIFEST_FILE);
            if layout.enclosing(index).is_some() && covered.contains(&manifest_path) {
                continue;
            }
            // Reached only when the enclosing root does not enumerate this
            // directory — a `.gitignore`d crate that cargo nonetheless builds.
            // Dropping it would leave the build using code nobody synced.
            warnings.push(format!(
                "{} is inside another root but not tracked by it; syncing it separately",
                root.path.display()
            ));
        }

        let mut idx = open_index(&root.path).map_err(ScanError::Other)?;
        // A nested root that got this far is one its enclosing repository does
        // not track, so neither git nor the ancestor ignore rules can describe
        // it (§Enumeration::Standalone).
        let scan = if root.nested {
            scanner::scan_with(&root.path, excludes, &mut idx, scanner::Enumeration::Standalone)?
        } else {
            scanner::scan(&root.path, excludes, &mut idx)?
        };
        if scan.attempts > 1 {
            tracing::info!(
                root = %root.path.display(),
                attempts = scan.attempts,
                "settled after a rescan (§4.2)"
            );
        }
        hashed += scan.hashed;
        reused += scan.reused;
        warnings.extend(scan.warnings.iter().cloned());

        let mut bytes = 0u64;
        let mut files = 0u32;
        for e in &scan.manifest.entries {
            let path = roots::anchored(&root.mount, &e.path);
            covered.insert(path.clone());
            bytes += e.size;
            files += 1;
            entries.push(FileEntry {
                path,
                // Only the primary root's baseline is materialised, so every
                // other root's content must travel through the CAS.
                in_baseline: root.primary && e.in_baseline,
                ..e.clone()
            });
        }
        seen_stats.extend(scanner::stat_entries(
            &layout.anchor,
            &scan
                .manifest
                .entries
                .iter()
                .map(|e| roots::anchored(&root.mount, &e.path))
                .collect::<Vec<_>>(),
        ));

        roots_info.push(RootInfo {
            mount: root.mount.clone(),
            local_path: root.path.to_string_lossy().into_owned(),
            primary: root.primary,
            bytes,
            files,
        });
        scanned.push(root.clone());
        if root.primary {
            primary = Some(scan);
        }
    }

    let primary = primary.ok_or_else(|| {
        ScanError::Other(anyhow::anyhow!("the primary root was not scanned"))
    })?;

    // Two roots must never claim the same path. `manifest::build` deduplicates
    // by path, so a collision would silently resolve to whichever entry sorted
    // first — one root's file quietly replaced by another's.
    let mut by_path: HashMap<&str, usize> = HashMap::new();
    for e in &entries {
        if by_path.insert(e.path.as_str(), 0).is_some() {
            return Err(ScanError::Other(anyhow::anyhow!(
                "two roots both provide `{}`; refusing to guess which one the build wants",
                e.path
            )));
        }
    }
    // Case conflicts across roots would otherwise only surface on the server,
    // after every byte had already been hashed and uploaded (§4.4).
    if let Some((a, b)) = rc_core::manifest::find_case_conflicts(&entries).into_iter().next() {
        return Err(ScanError::CaseConflict(a, b));
    }

    // One more sweep over everything: a root scanned early may have been edited
    // while a later one was still being read. Re-enumerate rather than re-stat
    // the paths already known — a file *created* in an early root after its own
    // scan finished is in neither set otherwise, and would be missing from the
    // manifest while present locally (§4.3).
    let mut after_paths: Vec<String> = Vec::new();
    for root in &scanned {
        let mode = if root.nested {
            scanner::Enumeration::Standalone
        } else {
            scanner::Enumeration::Auto
        };
        after_paths.extend(
            scanner::enumerate(&root.path, excludes, mode)?
                .into_iter()
                .map(|p| roots::anchored(&root.mount, &p)),
        );
    }
    let after = scanner::stat_entries(&layout.anchor, &after_paths);
    let mut changed: Vec<String> = seen_stats
        .keys()
        .chain(after.keys())
        .filter(|p| seen_stats.get(*p) != after.get(*p))
        .cloned()
        .collect();
    changed.sort();
    changed.dedup();

    // Judged by what was actually scanned, not by what the layout considered.
    // Cargo reports a workspace's own members as path dependencies, so an
    // ordinary single-repo project arrives here with a dozen nested roots that
    // all turn out to be covered. Counting those would mark the manifest
    // multi-root, demand the `multi-root` worker capability, and lock the
    // project out of every older worker — for a file layout that did not change
    // at all.
    let single = scanned.len() == 1 && layout.anchor_mount().is_empty();
    let manifest = rc_core::manifest::build_multi(
        entries,
        &primary.manifest.base_commit,
        primary.manifest.baseline,
        layout.anchor_mount(),
        if single { Vec::new() } else { roots_info },
    );

    Ok(Once {
        scan: MultiScan {
            manifest,
            anchor: layout.anchor.clone(),
            repo_url: primary.repo_url,
            is_git: primary.is_git,
            first_base_commit: primary.first_base_commit,
            warnings,
            scanned,
            hashed,
            reused,
        },
        unstable: (!changed.is_empty()).then_some(changed),
    })
}

/// Which discovered directories lie outside `root` — the ones whose contents
/// would leave the machine only because of this feature, and therefore the ones
/// that need the user's agreement (§privacy).
///
/// Only the outermost are returned. A crate inside a directory already on the
/// list travels with it, so asking about both would make the user approve the
/// same bytes twice and turn a one-line answer into four.
pub fn external_to(root: &Path, discovered: &[PathBuf]) -> Vec<PathBuf> {
    let outside: Vec<&PathBuf> = discovered.iter().filter(|p| !p.starts_with(root)).collect();
    outside
        .iter()
        .filter(|p| !outside.iter().any(|o| o != *p && p.starts_with(*o)))
        .map(|p| (*p).clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-mr-{tag}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn repo(base: &Path, name: &str) -> PathBuf {
        let d = base.join(name);
        std::fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "--quiet", "-b", "main"]);
        git(&d, &["config", "user.email", "t@example.com"]);
        git(&d, &["config", "user.name", "t"]);
        d
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn commit(dir: &Path) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "--quiet", "-m", "wip"]);
    }

    fn ex(dirs: &[&str]) -> Excludes {
        Excludes::structural(dirs)
    }

    fn indexes() -> impl FnMut(&Path) -> anyhow::Result<StatIndex> {
        |_: &Path| StatIndex::open_memory()
    }

    fn paths(m: &Manifest) -> Vec<String> {
        m.entries.iter().map(|e| e.path.clone()).collect()
    }

    #[test]
    fn a_single_root_produces_exactly_what_it_always_did() {
        // The compatibility guarantee, checked against the single-root scanner
        // rather than against a remembered constant.
        let base = scratch("single");
        let app = repo(&base, "app");
        write(&app, "src/main.rs", "fn main() {}");
        commit(&app);

        let app = app.canonicalize().unwrap();
        let layout = roots::compute(&app, &[]).unwrap();
        let multi = scan_all(&layout, &ex(&["target"]), indexes()).unwrap();

        let mut idx = StatIndex::open_memory().unwrap();
        let single = scanner::scan(&app, &ex(&["target"]), &mut idx).unwrap();

        assert_eq!(paths(&multi.manifest), paths(&single.manifest));
        assert_eq!(multi.manifest.root_hash, single.manifest.root_hash);
        assert_eq!(multi.manifest.anchor_mount, "");
        assert!(multi.manifest.roots.is_empty(), "no multi-root bookkeeping for one root");
    }

    #[test]
    fn a_sibling_root_is_prefixed_and_the_relative_position_survives() {
        let base = scratch("sibling");
        let app = repo(&base, "app");
        let lib = repo(&base, "lib");
        write(&app, "src/main.rs", "fn main() {}");
        write(&lib, "src/lib.rs", "pub fn f() {}");
        commit(&app);
        commit(&lib);

        let app = app.canonicalize().unwrap();
        let lib = lib.canonicalize().unwrap();
        let layout = roots::compute(&app, std::slice::from_ref(&lib)).unwrap();
        let scan = scan_all(&layout, &ex(&["target"]), indexes()).unwrap();

        assert_eq!(scan.manifest.anchor_mount, "app");
        let p = paths(&scan.manifest);
        assert!(p.contains(&"app/src/main.rs".to_string()), "{p:?}");
        assert!(p.contains(&"lib/src/lib.rs".to_string()), "{p:?}");
        assert_eq!(scan.anchor, base.canonicalize().unwrap());
        assert_eq!(scan.manifest.roots.len(), 2);
        rc_core::manifest::validate(&scan.manifest).unwrap();
    }

    #[test]
    fn only_the_primary_root_rides_the_git_baseline() {
        // The extra root has its own commits, but the worker materialises only
        // the primary's baseline; anything else must come through the CAS.
        let base = scratch("baseline");
        let app = repo(&base, "app");
        let lib = repo(&base, "lib");
        write(&app, "a.rs", "clean");
        write(&lib, "b.rs", "also clean");
        commit(&app);
        commit(&lib);

        let layout = roots::compute(
            &app.canonicalize().unwrap(),
            &[lib.canonicalize().unwrap()],
        )
        .unwrap();
        let scan = scan_all(&layout, &ex(&[]), indexes()).unwrap();

        let by_path: HashMap<&str, &FileEntry> =
            scan.manifest.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        assert!(by_path["app/a.rs"].in_baseline, "the primary keeps its L1 saving");
        assert!(!by_path["lib/b.rs"].in_baseline, "an extra root is L2-only");
        assert!(rc_core::manifest::blobs_to_reconcile(&scan.manifest)
            .len()
            .eq(&1));
        rc_core::manifest::validate(&scan.manifest).unwrap();
    }

    #[test]
    fn a_nested_root_the_outer_repo_tracks_is_not_scanned_twice() {
        let base = scratch("nested-covered");
        let app = repo(&base, "app");
        write(&app, "src/main.rs", "fn main() {}");
        write(&app, "vendor/inner/Cargo.toml", "[package]\nname = \"inner\"\n");
        write(&app, "vendor/inner/src/lib.rs", "pub fn f() {}");
        commit(&app);

        let app = app.canonicalize().unwrap();
        let inner = app.join("vendor/inner");
        let layout = roots::compute(&app, &[inner]).unwrap();
        let scan = scan_all(&layout, &ex(&[]), indexes()).unwrap();

        assert_eq!(scan.scanned.len(), 1, "the outer scan already covers it");
        let p = paths(&scan.manifest);
        assert_eq!(p.iter().filter(|x| x.contains("inner/src/lib.rs")).count(), 1);
        assert_eq!(p.iter().filter(|x| *x == "vendor/inner/src/lib.rs").count(), 1);
    }

    #[test]
    fn a_nested_root_the_outer_repo_ignores_is_scanned_separately() {
        // The case that makes "inside the primary root" the wrong test for
        // "already synced": cargo builds it, git never lists it.
        let base = scratch("nested-ignored");
        let app = repo(&base, "app");
        write(&app, ".gitignore", "local-crates/\n");
        write(&app, "src/main.rs", "fn main() {}");
        commit(&app);
        write(&app, "local-crates/foo/Cargo.toml", "[package]\nname = \"foo\"\n");
        write(&app, "local-crates/foo/src/lib.rs", "pub fn f() {}");

        let app = app.canonicalize().unwrap();
        let plain = {
            let mut idx = StatIndex::open_memory().unwrap();
            scanner::scan(&app, &ex(&[]), &mut idx).unwrap()
        };
        assert!(
            !paths(&plain.manifest).iter().any(|p| p.contains("local-crates")),
            "precondition: the ignored crate is invisible to the primary scan"
        );

        let layout = roots::compute(&app, &[app.join("local-crates/foo")]).unwrap();
        let scan = scan_all(&layout, &ex(&[]), indexes()).unwrap();

        assert_eq!(scan.scanned.len(), 2, "it has to be picked up on its own");
        assert!(
            paths(&scan.manifest).contains(&"local-crates/foo/src/lib.rs".to_string()),
            "{:?}",
            paths(&scan.manifest)
        );
        assert!(scan.warnings.iter().any(|w| w.contains("not tracked by it")));
    }

    #[test]
    fn editing_an_extra_root_changes_the_root_hash() {
        // Without this the fingerprint would serve a stale result after a real
        // change to a dependency's source.
        let base = scratch("hashchange");
        let app = repo(&base, "app");
        let lib = repo(&base, "lib");
        write(&app, "a.rs", "x");
        write(&lib, "b.rs", "before");
        commit(&app);
        commit(&lib);

        let app = app.canonicalize().unwrap();
        let lib = lib.canonicalize().unwrap();
        let layout = roots::compute(&app, std::slice::from_ref(&lib)).unwrap();
        let first = scan_all(&layout, &ex(&[]), indexes()).unwrap();

        write(&lib, "b.rs", "after");
        let second = scan_all(&layout, &ex(&[]), indexes()).unwrap();
        assert_ne!(first.manifest.root_hash, second.manifest.root_hash);
    }

    #[test]
    fn an_ordinary_workspace_stays_single_root_however_many_members_it_has() {
        // Cargo reports a workspace's own members as path dependencies, so they
        // all arrive here as nested roots. They are covered by the repository's
        // own enumeration, so the manifest must come out exactly as it would
        // without them — otherwise every ordinary project would suddenly demand
        // a multi-root-capable worker.
        let base = scratch("members");
        let app = repo(&base, "app");
        write(&app, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
        for member in ["a", "b"] {
            write(&app, &format!("{member}/Cargo.toml"), &format!("[package]\nname = \"{member}\"\n"));
            write(&app, &format!("{member}/src/lib.rs"), "");
        }
        commit(&app);

        let app = app.canonicalize().unwrap();
        let layout = roots::compute(&app, &[app.join("a"), app.join("b")]).unwrap();
        assert_eq!(layout.roots.len(), 3, "all three are considered");

        let scan = scan_all(&layout, &ex(&[]), indexes()).unwrap();
        assert_eq!(scan.scanned.len(), 1, "but only the repository is scanned");
        assert_eq!(scan.manifest.anchor_mount, "");
        assert!(
            scan.manifest.roots.is_empty(),
            "so the manifest must not look multi-root: {:?}",
            scan.manifest.roots
        );
        assert!(paths(&scan.manifest).contains(&"a/src/lib.rs".to_string()));
    }

    #[test]
    fn two_roots_claiming_the_same_path_is_an_error_not_a_silent_choice() {
        let base = scratch("collide");
        let app = repo(&base, "app");
        write(&app, "src/main.rs", "fn main() {}");
        commit(&app);
        let app = app.canonicalize().unwrap();

        // A nested root the repository does not track, holding a file at the
        // same anchor-relative path the outer scan also produced.
        write(&app, "src/lib.rs", "pub fn f() {}");
        let layout = roots::compute(&app, &[app.join("src")]).unwrap();
        // `src` has no Cargo.toml, so it is treated as uncovered and scanned
        // standalone — which re-reads main.rs and collides.
        let Err(err) = scan_all(&layout, &ex(&[]), indexes()) else {
            panic!("a collision must not resolve silently");
        };
        assert!(
            err.to_string().contains("both provide"),
            "a collision must be reported, got: {err}"
        );
    }

    #[test]
    fn external_roots_are_the_ones_outside_the_primary() {
        let root = PathBuf::from("/a/app");
        let found = vec![
            PathBuf::from("/a/app/inner"),
            PathBuf::from("/a/lib"),
            PathBuf::from("/b/other"),
        ];
        assert_eq!(
            external_to(&root, &found),
            vec![PathBuf::from("/a/lib"), PathBuf::from("/b/other")]
        );
    }
}
