//! Workspace manifests.
//!
//! The manifest is the *single source of truth* for workspace reconstruction
//! (§7.3): a file that exists in the git baseline but not in the manifest must
//! be deleted on the worker, otherwise "remote compiles, local doesn't" bugs
//! appear. It therefore records mode (+x) and symlink targets too (§4.4).

use crate::pb::{EntryType, FileEntry, Manifest};
use std::collections::BTreeMap;

/// Build a canonical single-root manifest from an unordered entry list.
pub fn build(entries: Vec<FileEntry>, base_commit: &str, baseline: bool) -> Manifest {
    build_multi(entries, base_commit, baseline, "", Vec::new())
}

/// Build a manifest that may span several local directories (§multi-root).
///
/// Entry paths are relative to the *anchor* — the deepest common ancestor of
/// every root — so one flat entry list describes the whole tree and every
/// existing consumer keeps working unchanged. With a single root the anchor is
/// that root, `anchor_mount` is empty, and the paths are byte-identical to what
/// this has always produced; `root_hash` therefore does not move and cached
/// results stay valid.
pub fn build_multi(
    mut entries: Vec<FileEntry>,
    base_commit: &str,
    baseline: bool,
    anchor_mount: &str,
    roots: Vec<crate::pb::RootInfo>,
) -> Manifest {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    let root_hash = root_hash(&entries);
    Manifest {
        entries,
        root_hash,
        base_commit: base_commit.to_string(),
        baseline,
        anchor_mount: anchor_mount.to_string(),
        roots,
    }
}

/// blake3 over the canonical serialization of the entry list.
///
/// Every field that changes build behaviour participates: path, size, content
/// hash, entry kind and the executable bit.
pub fn root_hash(entries: &[FileEntry]) -> String {
    let mut sorted: Vec<&FileEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = blake3::Hasher::new();
    for e in sorted {
        hasher.update(e.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(e.size.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(e.hash.as_bytes());
        hasher.update(b"\0");
        hasher.update(&[e.r#type as u8, u8::from(e.executable)]);
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

/// Paths differing only by case collide on a case-insensitive checkout
/// (macOS dev box) but not on the Linux worker. Silently overwriting one with
/// the other produces a workspace nobody can reproduce, so we refuse (§4.4).
pub fn find_case_conflicts(entries: &[FileEntry]) -> Vec<(String, String)> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for e in entries {
        let key = e.path.to_lowercase();
        match seen.get(&key) {
            Some(prev) if *prev != e.path => {
                conflicts.push(((*prev).to_string(), e.path.clone()));
            }
            Some(_) => {}
            None => {
                seen.insert(key, &e.path);
            }
        }
    }
    conflicts
}

/// Hashes the agent must make sure the server holds before submitting.
/// Entries covered by the L1 baseline are skipped — the worker gets those
/// from its git mirror.
pub fn blobs_to_reconcile(m: &Manifest) -> Vec<String> {
    let mut out: Vec<String> = m
        .entries
        .iter()
        .filter(|e| e.r#type == EntryType::EntryFile as i32)
        .filter(|e| !(m.baseline && e.in_baseline))
        .filter(|e| e.size > 0)
        .map(|e| e.hash.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Total bytes the L2 layer would move if nothing were cached.
pub fn dirty_bytes(m: &Manifest) -> u64 {
    m.entries
        .iter()
        .filter(|e| !(m.baseline && e.in_baseline))
        .map(|e| e.size)
        .sum()
}

/// Hash of a symlink is the hash of its *target string*: we never follow
/// symlinks, so the target text is the content (§4.4).
pub fn symlink_hash(target: &str) -> String {
    blake3::hash(target.as_bytes()).to_hex().to_string()
}

/// Rejects paths that would escape the workspace root during reconstruction.
pub fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return false;
    }
    // Windows drive prefixes and UNC paths.
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        return false;
    }
    !path
        .split('/')
        .any(|c| c == ".." || c == "." || c.is_empty())
}

/// Whether an anchor-relative path lies inside `mount`. An empty mount is the
/// anchor itself and contains everything.
pub fn under_mount(path: &str, mount: &str) -> bool {
    mount.is_empty() || path.strip_prefix(mount).is_some_and(|rest| rest.starts_with('/'))
}

/// Validate an incoming manifest before it is allowed to touch a worker.
pub fn validate(m: &Manifest) -> Result<(), String> {
    for e in &m.entries {
        if !is_safe_relative_path(&e.path) {
            return Err(format!("unsafe path in manifest: {}", e.path));
        }
        if e.r#type == EntryType::EntryFile as i32 && e.hash.len() != 64 {
            return Err(format!("bad blob hash for {}: {}", e.path, e.hash));
        }
        if e.r#type == EntryType::EntrySymlink as i32 {
            // An agent predating `symlink_target` sends nothing here, and the
            // worker used to fall back to `hash` — producing a link pointing at
            // a 64-hex string. Refusing is far better than rebuilding that.
            if e.symlink_target.is_empty() {
                return Err(format!(
                    "symlink {} carries no target; the agent is too old to sync symlinks correctly",
                    e.path
                ));
            }
            if e.hash != symlink_hash(&e.symlink_target) {
                return Err(format!(
                    "symlink {} has a hash that does not match its target",
                    e.path
                ));
            }
        }
    }
    let conflicts = find_case_conflicts(&m.entries);
    if let Some((a, b)) = conflicts.first() {
        return Err(format!("case-conflicting paths: {a} vs {b}"));
    }
    // Mounts become directory prefixes on the worker, so they are held to the
    // same standard as entry paths. Empty means "the anchor itself", which is
    // what a single-root sync always uses.
    if !m.anchor_mount.is_empty() && !is_safe_relative_path(&m.anchor_mount) {
        return Err(format!("unsafe anchor mount: {}", m.anchor_mount));
    }
    for root in &m.roots {
        if !root.mount.is_empty() && !is_safe_relative_path(&root.mount) {
            return Err(format!("unsafe root mount: {}", root.mount));
        }
    }
    if !m.roots.is_empty() {
        let primaries = m.roots.iter().filter(|r| r.primary).count();
        if primaries != 1 {
            return Err(format!("manifest declares {primaries} primary roots, expected exactly 1"));
        }
        let primary = m.roots.iter().find(|r| r.primary).expect("checked above");
        if primary.mount != m.anchor_mount {
            return Err(format!(
                "anchor mount `{}` disagrees with the primary root's mount `{}`",
                m.anchor_mount, primary.mount
            ));
        }
        // The L1 baseline is extracted under the primary's mount, so entries
        // belonging to any other root must not claim to come from it — the
        // worker would skip fetching their content from the CAS and then find
        // nothing on disk.
        //
        // Ownership is by *longest* matching mount, not by the anchor: when the
        // primary is itself the anchor its mount is empty and matches
        // everything, so a nested root's entries would otherwise pass.
        for e in &m.entries {
            if !e.in_baseline {
                continue;
            }
            let owner = m
                .roots
                .iter()
                .filter(|r| under_mount(&e.path, &r.mount))
                .max_by_key(|r| r.mount.len());
            match owner {
                Some(r) if r.primary => {}
                _ => {
                    return Err(format!(
                        "entry {} does not belong to the primary root but claims the git baseline",
                        e.path
                    ))
                }
            }
        }
    }
    let expect = root_hash(&m.entries);
    if expect != m.root_hash {
        return Err(format!(
            "manifest root hash mismatch: declared {} computed {}",
            m.root_hash, expect
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, hash: &str) -> FileEntry {
        FileEntry {
            path: path.into(),
            size: 10,
            hash: hash.into(),
            r#type: EntryType::EntryFile as i32,
            executable: false,
            in_baseline: false,
            symlink_target: String::new(),
        }
    }

    #[test]
    fn root_hash_is_order_independent() {
        let a = vec![file("a.rs", "1"), file("b.rs", "2")];
        let b = vec![file("b.rs", "2"), file("a.rs", "1")];
        assert_eq!(root_hash(&a), root_hash(&b));
    }

    #[test]
    fn root_hash_tracks_the_executable_bit() {
        let plain = vec![file("x.sh", "1")];
        let mut exec = plain.clone();
        exec[0].executable = true;
        assert_ne!(root_hash(&plain), root_hash(&exec));
    }

    #[test]
    fn root_hash_tracks_deletions() {
        let two = vec![file("a.rs", "1"), file("b.rs", "2")];
        let one = vec![file("a.rs", "1")];
        assert_ne!(root_hash(&two), root_hash(&one));
    }

    #[test]
    fn case_conflicts_are_detected() {
        let entries = vec![file("src/Main.rs", "1"), file("src/main.rs", "2")];
        assert_eq!(find_case_conflicts(&entries).len(), 1);
    }

    #[test]
    fn traversal_paths_are_rejected() {
        assert!(!is_safe_relative_path("../etc/passwd"));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path("a//b"));
        assert!(!is_safe_relative_path("C:/win"));
        assert!(is_safe_relative_path("src/main.rs"));
    }

    #[test]
    fn baseline_entries_skip_reconcile() {
        let mut entries = vec![file("a.rs", "a".repeat(64).as_str()), file("b.rs", "b".repeat(64).as_str())];
        entries[0].in_baseline = true;
        let m = build(entries, "abc", true);
        assert_eq!(blobs_to_reconcile(&m), vec!["b".repeat(64)]);
    }

    #[test]
    fn full_l2_fallback_reconciles_everything() {
        let mut entries = vec![file("a.rs", &"a".repeat(64)), file("b.rs", &"b".repeat(64))];
        entries[0].in_baseline = true;
        // baseline unusable => every entry must travel through the CAS
        let m = build(entries, "", false);
        assert_eq!(blobs_to_reconcile(&m).len(), 2);
    }

    fn root_info(mount: &str, primary: bool) -> crate::pb::RootInfo {
        crate::pb::RootInfo {
            mount: mount.into(),
            local_path: format!("/local/{mount}"),
            primary,
            bytes: 0,
            files: 0,
        }
    }

    #[test]
    fn an_extra_roots_entry_cannot_claim_the_primarys_git_baseline() {
        // The worker extracts the baseline only under the primary's mount, so
        // a baseline claim elsewhere means content that is fetched from nowhere.
        let mut theirs = file("lib/src/a.rs", &"a".repeat(64));
        theirs.in_baseline = true;
        let m = build_multi(
            vec![theirs],
            "abc",
            true,
            "app",
            vec![root_info("app", true), root_info("lib", false)],
        );
        let err = validate(&m).unwrap_err();
        assert!(err.contains("claims the git baseline"), "{err}");
    }

    #[test]
    fn a_nested_root_cannot_claim_the_baseline_even_when_the_primary_is_the_anchor() {
        // The primary's mount is empty here, so a naive prefix test would say
        // every path belongs to it. Ownership has to go to the longest match.
        let mut theirs = file("vendor/foo/src/a.rs", &"a".repeat(64));
        theirs.in_baseline = true;
        let m = build_multi(
            vec![theirs],
            "abc",
            true,
            "",
            vec![root_info("", true), root_info("vendor/foo", false)],
        );
        assert!(validate(&m).is_err());
    }

    #[test]
    fn the_primarys_own_entries_may_ride_the_baseline() {
        let mut mine = file("app/src/a.rs", &"a".repeat(64));
        mine.in_baseline = true;
        let m = build_multi(
            vec![mine],
            "abc",
            true,
            "app",
            vec![root_info("app", true), root_info("lib", false)],
        );
        validate(&m).unwrap();
    }

    #[test]
    fn a_manifest_must_declare_exactly_one_primary_root() {
        let m = build_multi(
            vec![file("app/a.rs", &"a".repeat(64))],
            "",
            false,
            "app",
            vec![root_info("app", true), root_info("lib", true)],
        );
        assert!(validate(&m).unwrap_err().contains("primary roots"));
    }

    #[test]
    fn an_unsafe_mount_is_refused() {
        for evil in ["../etc", "/etc", "a/../b"] {
            let m = build_multi(
                vec![file("a.rs", &"a".repeat(64))],
                "",
                false,
                evil,
                vec![root_info(evil, true)],
            );
            assert!(validate(&m).is_err(), "`{evil}` must not pass as a mount");
        }
    }

    #[test]
    fn validate_catches_a_forged_root_hash() {
        let mut m = build(vec![file("a.rs", &"a".repeat(64))], "", false);
        m.root_hash = "deadbeef".into();
        assert!(validate(&m).is_err());
    }
}
