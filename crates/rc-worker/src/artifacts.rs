//! Host side of the artifact collect step (docs/proposals/build-artifacts.md §4).
//!
//! The sandbox has already dereferenced every symlink and copied the results
//! into a fresh directory. This side trusts none of that: it walks without
//! following links, takes regular files only, and enforces the limits again.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Relative, `/`-separated.
    pub rel: String,
    pub path: PathBuf,
    pub size: u64,
    pub mode: u32,
}

/// Regular files under `out`, sorted, within `max_files` and `max_bytes`.
/// Anything else is named in the returned notes rather than silently dropped.
pub fn gather(out: &Path, max_files: usize, max_bytes: u64) -> io::Result<(Vec<Found>, Vec<String>)> {
    let mut found = Vec::new();
    let mut notes = Vec::new();
    walk(out, out, &mut found, &mut notes)?;
    found.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut kept = Vec::new();
    let mut total = 0u64;
    for f in found {
        if kept.len() >= max_files {
            notes.push(format!("over file limit: {}", f.rel));
        } else if total + f.size > max_bytes {
            notes.push(format!("over size limit: {}", f.rel));
        } else {
            total += f.size;
            kept.push(f);
        }
    }
    Ok((kept, notes))
}

/// Remove a scratch tree even if something in it was made unreadable.
pub fn remove_scratch(dir: &Path) {
    fn unlock(dir: &Path) {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    unlock(&e.path());
                }
            }
        }
    }
    if dir.exists() {
        unlock(dir);
        let _ = fs::remove_dir_all(dir);
    }
}

/// Bytes under `dir`, links not followed, unreadable parts skipped — the
/// watchdog's estimate while the collect step is still writing.
pub fn usage(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    let mut total = 0;
    for e in rd.flatten() {
        let Ok(meta) = fs::symlink_metadata(e.path()) else { continue };
        if meta.is_dir() {
            total += usage(&e.path());
        } else {
            total += meta.len();
        }
    }
    total
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<Found>, notes: &mut Vec<String>) -> io::Result<()> {
    // The container ran as this uid and could have locked us out of our own
    // scratch; taking the permission back costs nothing.
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // symlink_metadata: a link is reported as a link, never followed.
        let meta = fs::symlink_metadata(&path)?;
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let ft = meta.file_type();
        if ft.is_dir() {
            walk(root, &path, found, notes)?;
        } else if ft.is_file() && !rc_core::artifacts::is_safe_relative(&rel) {
            notes.push("skipped a file with an unusable name".into());
        } else if ft.is_file() {
            found.push(Found {
                rel,
                path,
                size: meta.len(),
                mode: meta.permissions().mode() & 0o777,
            });
        } else {
            notes.push(format!("skipped non-regular file: {rel}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rc-artifacts-{tag}-{}", ulid::Ulid::generate()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn links_are_never_followed_and_limits_hold() {
        let out = tmp("gather");
        fs::create_dir_all(out.join("target/release")).unwrap();
        fs::write(out.join("target/release/app"), vec![0u8; 10]).unwrap();
        fs::set_permissions(out.join("target/release/app"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(out.join("b"), vec![0u8; 10]).unwrap();
        fs::write(out.join("c"), vec![0u8; 10]).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", out.join("leak")).unwrap();

        let (kept, notes) = gather(&out, 10, 25).unwrap();
        let names: Vec<_> = kept.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(names, vec!["b", "c"]);
        assert!(notes.iter().any(|n| n.contains("leak")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("over size limit: target/release/app")), "{notes:?}");

        let (kept, _) = gather(&out, 1, 1 << 20).unwrap();
        assert_eq!(kept.len(), 1);
        let (kept, _) = gather(&out, 10, 1 << 20).unwrap();
        let app = kept.iter().find(|f| f.rel == "target/release/app").unwrap();
        assert_eq!(app.mode, 0o755);
    }
}
