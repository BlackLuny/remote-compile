//! Workspace manifests.
//!
//! The manifest is the *single source of truth* for workspace reconstruction
//! (§7.3): a file that exists in the git baseline but not in the manifest must
//! be deleted on the worker, otherwise "remote compiles, local doesn't" bugs
//! appear. It therefore records mode (+x) and symlink targets too (§4.4).

use crate::pb::{EntryType, FileEntry, Manifest};
use std::collections::BTreeMap;

/// Build a canonical manifest from an unordered entry list.
pub fn build(mut entries: Vec<FileEntry>, base_commit: &str, baseline: bool) -> Manifest {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    let root_hash = root_hash(&entries);
    Manifest {
        entries,
        root_hash,
        base_commit: base_commit.to_string(),
        baseline,
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

/// Validate an incoming manifest before it is allowed to touch a worker.
pub fn validate(m: &Manifest) -> Result<(), String> {
    for e in &m.entries {
        if !is_safe_relative_path(&e.path) {
            return Err(format!("unsafe path in manifest: {}", e.path));
        }
        if e.r#type == EntryType::EntryFile as i32 && e.hash.len() != 64 {
            return Err(format!("bad blob hash for {}: {}", e.path, e.hash));
        }
    }
    let conflicts = find_case_conflicts(&m.entries);
    if let Some((a, b)) = conflicts.first() {
        return Err(format!("case-conflicting paths: {a} vs {b}"));
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

    #[test]
    fn validate_catches_a_forged_root_hash() {
        let mut m = build(vec![file("a.rs", &"a".repeat(64))], "", false);
        m.root_hash = "deadbeef".into();
        assert!(validate(&m).is_err());
    }
}
