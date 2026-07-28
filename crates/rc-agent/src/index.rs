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
        // `summary` used to hold the bare result kind, which made a cache hit
        // strictly less informative than the miss that produced it — an
        // env_error replayed as the word "env_error", without the missing
        // dependency the first answer named.
        //
        // The whole result is stored instead, as data rather than as rendered
        // text: rendering at write time would freeze that invocation's
        // `max_diagnostics` and its `synced=` byte count into an answer replayed
        // later under different settings, by a hit that synced nothing.
        // An older row has kind = '' and is read the old way.
        let _ = conn.execute("ALTER TABLE results ADD COLUMN kind TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE results ADD COLUMN result TEXT NOT NULL DEFAULT ''", []);
        Ok(ResultCache { conn })
    }

    pub fn put(&self, fingerprint: &str, task_id: &str, result: &rc_core::pb::TaskResult) -> Result<()> {
        // Same cacheable set as the server (§1.3): only success / compile_error.
        // Caching OOM or env failures would freeze a resource verdict for 24h.
        let kind = rc_core::ResultKind::parse_or_default(&result.kind);
        if !kind.is_cacheable() {
            return Ok(());
        }
        let json = serde_json::to_string(result).unwrap_or_default();
        self.conn.execute(
            "INSERT INTO results (fingerprint, task_id, summary, kind, result, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(fingerprint) DO UPDATE SET
                task_id = ?2, summary = ?3, kind = ?4, result = ?5, at = ?6",
            params![fingerprint, task_id, &result.kind, &result.kind, json, rc_core::now_secs()],
        )?;
        Ok(())
    }

    /// Entries older than the TTL are ignored: builds are not perfectly
    /// deterministic, so an unbounded cache can serve a stale "success"
    /// (§5.1, risk #16).
    ///
    /// Returns the task id, the result kind, and the stored result when there
    /// is one — a row written before the result was kept has only the kind.
    pub fn get(
        &self,
        fingerprint: &str,
        ttl_secs: i64,
    ) -> Option<(String, String, Option<rc_core::pb::TaskResult>)> {
        let cutoff = rc_core::now_secs() - ttl_secs;
        let (task_id, summary, kind, json): (String, String, String, String) = self
            .conn
            .query_row(
                "SELECT task_id, summary, kind, result FROM results
                 WHERE fingerprint = ?1 AND at > ?2",
                params![fingerprint, cutoff],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .ok()
            .flatten()?;
        // Pre-upgrade row: `summary` held the kind and nothing else. That is
        // all it ever had, so replaying it loses nothing.
        if kind.is_empty() {
            return Some((task_id, summary, None));
        }
        match serde_json::from_str(&json) {
            Ok(result) => Some((task_id, kind, Some(result))),
            // A row carrying a kind but no result comes from a build that
            // stored pre-rendered text instead. Replaying just the kind would
            // be worse than the miss it stands in for, so treat it as a miss
            // and let the task run again.
            Err(_) => None,
        }
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
        let ok = rc_core::pb::TaskResult { kind: "success".into(), ..Default::default() };
        cache.put("fp", "t1", &ok).unwrap();
        assert_eq!(cache.get("fp", 3600).unwrap().0, "t1");
        // Risk #16: a stale success must expire rather than mislead.
        assert!(cache.get("fp", -1).is_none());
    }

    #[test]
    fn a_cache_hit_replays_the_whole_verdict_not_just_its_kind() {
        // Stored as data, not as rendered text: rendering at write time freezes
        // that call's `max_diagnostics` and its `synced=` byte count into an
        // answer replayed later by a hit that synced nothing.
        //
        // Only success/compile_error are cacheable (§1.3); env_error (OOM etc.)
        // must not stick for 24h.
        let cache = ResultCache::open_memory().unwrap();
        let env = rc_core::pb::TaskResult {
            kind: "env_error".into(),
            summary: "环境错误（exit 101）".into(),
            env_hints: vec!["  - pkg-config 模块 `librrd` 未找到".into()],
            ..Default::default()
        };
        cache.put("fp-env", "t9", &env).unwrap();
        assert!(cache.get("fp-env", 3600).is_none(), "env_error must not be cached");

        let compile = rc_core::pb::TaskResult {
            kind: "compile_error".into(),
            summary: "1 errors".into(),
            diagnostics: vec![rc_core::pb::Diagnostic {
                level: "error".into(),
                code: "E0308".into(),
                message: "mismatched types".into(),
                file: "src/lib.rs".into(),
                line: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        cache.put("fp", "t9", &compile).unwrap();
        let (task_id, kind, cached) = cache.get("fp", 3600).unwrap();
        assert_eq!((task_id.as_str(), kind.as_str()), ("t9", "compile_error"));
        let cached = cached.expect("the result itself must come back");
        assert_eq!(cached.diagnostics.len(), 1);
        assert_eq!(cached.diagnostics[0].code, "E0308");
    }

    #[test]
    fn a_row_written_before_the_new_columns_still_reads() {
        // The cache predates both columns; an entry written by the older agent
        // must not start reporting a bogus kind after an upgrade.
        let cache = ResultCache::open_memory().unwrap();
        cache
            .conn
            .execute(
                "INSERT INTO results (fingerprint, task_id, summary, at) VALUES ('fp','t1','success',?1)",
                params![rc_core::now_secs()],
            )
            .unwrap();
        let (task_id, kind, cached) = cache.get("fp", 3600).unwrap();
        assert_eq!((task_id.as_str(), kind.as_str()), ("t1", "success"));
        assert!(cached.is_none(), "an old row carries no result to replay");
    }

    #[test]
    fn a_row_holding_only_rendered_text_is_treated_as_a_miss() {
        // An intermediate build stored the rendered string and a kind but no
        // result. Replaying just the kind would be less useful than the miss it
        // stands in for, so the task runs again instead.
        let cache = ResultCache::open_memory().unwrap();
        cache
            .conn
            .execute(
                "INSERT INTO results (fingerprint, task_id, summary, kind, result, at)
                 VALUES ('fp','t1','✗ 环境错误 … librrd-dev …','env_error','',?1)",
                params![rc_core::now_secs()],
            )
            .unwrap();
        assert!(cache.get("fp", 3600).is_none());
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
