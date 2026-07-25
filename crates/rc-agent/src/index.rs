//! Per-worktree stat index and the local result cache (§4.5).
//!
//! The index turns a rescan into a stat walk: only files whose (size, mtime)
//! changed get re-hashed. mtime is a *hint*, never a verdict — the stored hash
//! is only reused when size and mtime both match, and the policy leans toward
//! re-hashing, because an unnecessary hash costs milliseconds while a missed
//! change is a wrong answer (§4.4).

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct StatIndex {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: String,
}

impl StatIndex {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL plus a busy timeout: several agent processes may share one
        // worktree index (§4.5).
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                path     TEXT PRIMARY KEY,
                size     INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                hash     TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        )?;
        Ok(StatIndex { conn })
    }

    /// Cached hash for a path, but only when size *and* mtime still agree.
    pub fn lookup(&self, path: &str, size: u64, mtime_ns: i64) -> Option<String> {
        self.conn
            .query_row(
                "SELECT size, mtime_ns, hash FROM files WHERE path = ?1",
                params![path],
                |r| {
                    Ok(Stat {
                        size: r.get::<_, i64>(0)? as u64,
                        mtime_ns: r.get(1)?,
                        hash: r.get(2)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
            .filter(|s| s.size == size && s.mtime_ns == mtime_ns)
            .map(|s| s.hash)
    }

    pub fn record_all(&mut self, entries: &[(String, Stat)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (path, stat) in entries {
            tx.execute(
                "INSERT INTO files (path, size, mtime_ns, hash) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET size = ?2, mtime_ns = ?3, hash = ?4",
                params![path, stat.size as i64, stat.mtime_ns, stat.hash],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop rows for files that no longer exist, so a long-lived index does
    /// not grow without bound.
    pub fn retain(&mut self, live: &std::collections::HashSet<String>) -> Result<usize> {
        let stale: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT path FROM files")?;
            let all = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            all.into_iter().filter(|p| !live.contains(p)).collect()
        };
        let tx = self.conn.transaction()?;
        for path in &stale {
            tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
        }
        tx.commit()?;
        Ok(stale.len())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn len(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Fingerprint → last result, so a repeat check answers without touching the
/// network at all (§5.1).
pub struct ResultCache {
    conn: Connection,
}

impl ResultCache {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS results (
                fingerprint TEXT PRIMARY KEY,
                task_id     TEXT NOT NULL,
                summary     TEXT NOT NULL,
                at          INTEGER NOT NULL
             );",
        )?;
        Ok(ResultCache { conn })
    }

    pub fn put(&self, fingerprint: &str, task_id: &str, summary: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO results (fingerprint, task_id, summary, at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(fingerprint) DO UPDATE SET task_id = ?2, summary = ?3, at = ?4",
            params![fingerprint, task_id, summary, rc_core::now_secs()],
        )?;
        Ok(())
    }

    /// Entries older than the TTL are ignored: builds are not perfectly
    /// deterministic, so an unbounded cache can serve a stale "success"
    /// (§5.1, risk #16).
    pub fn get(&self, fingerprint: &str, ttl_secs: i64) -> Option<(String, String)> {
        let cutoff = rc_core::now_secs() - ttl_secs;
        self.conn
            .query_row(
                "SELECT task_id, summary FROM results WHERE fingerprint = ?1 AND at > ?2",
                params![fingerprint, cutoff],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten()
    }
}

/// Hashes the control plane confirmed it holds. Purely an optimisation, with a
/// TTL far shorter than the server's GC window so a stale entry cannot make us
/// skip a required upload (§4.7).
pub struct KnownBlobs {
    conn: Connection,
}

pub const KNOWN_BLOB_TTL_SECS: i64 = 7 * 24 * 3600;

impl KnownBlobs {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS known (hash TEXT PRIMARY KEY, at INTEGER NOT NULL);",
        )?;
        Ok(KnownBlobs { conn })
    }

    pub fn note(&mut self, hashes: &[String]) -> Result<()> {
        let now = rc_core::now_secs();
        let tx = self.conn.transaction()?;
        for h in hashes {
            tx.execute(
                "INSERT INTO known (hash, at) VALUES (?1, ?2) ON CONFLICT(hash) DO UPDATE SET at = ?2",
                params![h, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn forget(&self, hash: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM known WHERE hash = ?1", params![hash])?;
        Ok(())
    }

    /// Split hashes into "the server almost certainly has these" and "must
    /// ask". Only the second group goes on the wire.
    pub fn partition(&self, hashes: &[String]) -> (Vec<String>, Vec<String>) {
        let cutoff = rc_core::now_secs() - KNOWN_BLOB_TTL_SECS;
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for h in hashes {
            let fresh: Option<i64> = self
                .conn
                .query_row(
                    "SELECT at FROM known WHERE hash = ?1 AND at > ?2",
                    params![h, cutoff],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten();
            if fresh.is_some() {
                known.push(h.clone());
            } else {
                unknown.push(h.clone());
            }
        }
        (known, unknown)
    }
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_hash_is_reused_only_when_size_and_mtime_agree() {
        let mut idx = StatIndex::open_memory().unwrap();
        idx.record_all(&[(
            "a.rs".into(),
            Stat { size: 10, mtime_ns: 100, hash: "h1".into() },
        )])
        .unwrap();

        assert_eq!(idx.lookup("a.rs", 10, 100).as_deref(), Some("h1"));
        // A changed mtime means re-hash, even at the same size.
        assert!(idx.lookup("a.rs", 10, 200).is_none());
        // A changed size means re-hash, even at the same mtime.
        assert!(idx.lookup("a.rs", 11, 100).is_none());
        assert!(idx.lookup("unknown.rs", 1, 1).is_none());
    }

    #[test]
    fn recording_the_same_path_twice_updates_in_place() {
        let mut idx = StatIndex::open_memory().unwrap();
        idx.record_all(&[("a.rs".into(), Stat { size: 1, mtime_ns: 1, hash: "old".into() })])
            .unwrap();
        idx.record_all(&[("a.rs".into(), Stat { size: 2, mtime_ns: 2, hash: "new".into() })])
            .unwrap();
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.lookup("a.rs", 2, 2).as_deref(), Some("new"));
    }

    #[test]
    fn deleted_files_are_pruned_from_the_index() {
        let mut idx = StatIndex::open_memory().unwrap();
        idx.record_all(&[
            ("keep.rs".into(), Stat { size: 1, mtime_ns: 1, hash: "a".into() }),
            ("gone.rs".into(), Stat { size: 1, mtime_ns: 1, hash: "b".into() }),
        ])
        .unwrap();
        let live: HashSet<String> = ["keep.rs".to_string()].into_iter().collect();
        assert_eq!(idx.retain(&live).unwrap(), 1);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn meta_survives_round_trips() {
        let idx = StatIndex::open_memory().unwrap();
        idx.set_meta("base_commit", "abc123").unwrap();
        assert_eq!(idx.meta("base_commit").as_deref(), Some("abc123"));
        assert!(idx.meta("absent").is_none());
    }

    #[test]
    fn the_result_cache_honours_its_ttl() {
        let cache = ResultCache::open_memory().unwrap();
        cache.put("fp", "t1", "success").unwrap();
        assert_eq!(cache.get("fp", 3600).unwrap().0, "t1");
        // Risk #16: a stale success must expire rather than mislead.
        assert!(cache.get("fp", -1).is_none());
    }

    #[test]
    fn known_blobs_split_into_ask_and_skip() {
        let mut known = KnownBlobs::open_memory().unwrap();
        known.note(&["a".repeat(64)]).unwrap();
        let (skip, ask) = known.partition(&["a".repeat(64), "b".repeat(64)]);
        assert_eq!(skip, vec!["a".repeat(64)]);
        assert_eq!(ask, vec!["b".repeat(64)]);
    }

    #[test]
    fn a_forgotten_blob_is_asked_about_again() {
        // §4.7: after a blob_missing report the local hint must not keep us
        // from re-uploading.
        let mut known = KnownBlobs::open_memory().unwrap();
        let h = "a".repeat(64);
        known.note(std::slice::from_ref(&h)).unwrap();
        known.forget(&h).unwrap();
        let (_, ask) = known.partition(std::slice::from_ref(&h));
        assert_eq!(ask, vec![h]);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_local_blob_hint_expires_well_before_server_gc() {
        // §4.7: 7 days here vs a 30-day server TTL leaves a wide margin.
        assert!(KNOWN_BLOB_TTL_SECS < 30 * 24 * 3600);
    }
}
