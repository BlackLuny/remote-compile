//! SQLite persistence (§18).
//!
//! SQLite has a single writer, so every hot-path write here has to stay
//! short. Anything high-frequency (worker heartbeat stats) deliberately lives
//! in memory instead and only reaches disk as a low-frequency `last_hb` bump
//! or a batched rollup (§15.1, risk #28).

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rc_core::{now_ms, now_secs};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub struct Store {
    conn: Mutex<Connection>,
}

/// Ordered migration steps. `MIGRATIONS[i]` takes a database at
/// `user_version == i` to `i + 1`, so **append only, never edit or reorder** —
/// a deployed database has already run the earlier entries.
///
/// Statements must be safe to apply to a database that already holds data.
/// SQLite's `ALTER TABLE ... ADD COLUMN` needs a non-`NULL` default when the
/// column is `NOT NULL`, and there is no `IF NOT EXISTS` for columns.
///
/// Step 0 is the initial schema, which `schema.sql` creates outright.
const MIGRATIONS: &[&str] = &[
    "",
    // Image health has to know *which* project an env_error came from, so that
    // one project's missing native dependency stops being charged to the image.
    "ALTER TABLE images ADD COLUMN last_env_error_project TEXT NOT NULL DEFAULT '';",
];

const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Bring one database from `from_version` to `steps.len()`, atomically.
///
/// `base_schema` is `CREATE TABLE IF NOT EXISTS ...`, so it fills in anything
/// absent and leaves existing tables untouched; `steps` carry the changes that
/// existing tables need.
fn apply_migrations(
    conn: &Connection,
    base_schema: &str,
    steps: &[&str],
    from_version: i64,
) -> Result<()> {
    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        conn.execute_batch(base_schema)?;
        for step in steps.iter().skip(from_version.max(0) as usize) {
            conn.execute_batch(step)?;
        }
        conn.pragma_update(None, "user_version", steps.len() as i64)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        // Half a migration is worse than none: a partially altered table is
        // not a state any code knows how to read.
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// --------------------------------------------------------------------------
// Row types. These are serialized straight onto the admin REST API, so field
// names are part of the frontend contract.
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskRow {
    pub id: String,
    pub task_type: String,
    pub project_id: String,
    pub worktree_id: String,
    pub agent_session: String,
    pub fingerprint: String,
    pub supersede_key: String,
    pub status: String,
    pub result_kind: String,
    pub command: String,
    pub image: String,
    pub log_ref: String,
    pub worker_id: String,
    pub attempt: i64,
    pub created_at: i64,
    pub started_at: i64,
    pub finished_at: i64,
    pub error: String,
    pub superseded_by: String,
    pub result_json: String,
    pub queue_ms: i64,
    pub sync_ms: i64,
    pub build_ms: i64,
    pub bytes_synced: i64,
    pub cache_hit: i64,
}

impl TaskRow {
    fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(TaskRow {
            id: r.get("id")?,
            task_type: r.get("task_type")?,
            project_id: r.get("project_id")?,
            worktree_id: r.get("worktree_id")?,
            agent_session: r.get("agent_session")?,
            fingerprint: r.get("fingerprint")?,
            supersede_key: r.get("supersede_key")?,
            status: r.get("status")?,
            result_kind: r.get("result_kind")?,
            command: r.get("command")?,
            image: r.get("image")?,
            log_ref: r.get("log_ref")?,
            worker_id: r.get("worker_id")?,
            attempt: r.get("attempt")?,
            created_at: r.get("created_at")?,
            started_at: r.get("started_at")?,
            finished_at: r.get("finished_at")?,
            error: r.get("error")?,
            superseded_by: r.get("superseded_by")?,
            result_json: r.get("result_json")?,
            queue_ms: r.get("queue_ms")?,
            sync_ms: r.get("sync_ms")?,
            build_ms: r.get("build_ms")?,
            bytes_synced: r.get("bytes_synced")?,
            cache_hit: r.get("cache_hit")?,
        })
    }

    pub fn result(&self) -> Option<rc_core::pb::TaskResult> {
        serde_json::from_str(&self.result_json).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectRow {
    pub id: String,
    pub repo_url: String,
    pub root_path: String,
    pub created_at: i64,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorktreeRow {
    pub id: String,
    pub project_id: String,
    pub label: String,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileRow {
    pub id: String,
    pub project_id: String,
    pub path: String,
    pub adapter: String,
    pub image: String,
    pub config_toml: String,
    pub created_by: String,
    pub last_success_at: i64,
    pub success_count: i64,
    pub total_count: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageRow {
    pub id: String,
    pub image_ref: String,
    pub digest: String,
    pub dockerfile: String,
    pub pull_ref: String,
    pub status: String,
    pub arch: String,
    pub targets: String,
    pub description: String,
    pub created_by: String,
    pub approved_by: String,
    pub approved_at: i64,
    pub last_success_at: i64,
    pub success_count: i64,
    pub total_count: i64,
    pub consecutive_env_errors: i64,
    pub built_at: i64,
    pub created_at: i64,
    pub build_log_ref: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerRow {
    pub id: String,
    pub arch: String,
    pub labels: String,
    pub capacity: String,
    pub status: String,
    pub version: String,
    pub max_parallel: i64,
    pub enrolled_at: i64,
    pub last_hb: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BlobRow {
    pub hash: String,
    pub size: i64,
    pub last_used: i64,
    pub pinned: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnrollmentTokenRow {
    pub token: String,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: i64,
    pub used_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditRow {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlertRow {
    pub id: i64,
    pub rule: String,
    pub level: String,
    pub message: String,
    pub at: i64,
    pub resolved_at: i64,
}

/// Filter for the Tasks page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskFilter {
    pub project_id: Option<String>,
    pub worktree_id: Option<String>,
    pub agent_session: Option<String>,
    pub status: Option<String>,
    pub result_kind: Option<String>,
    pub task_type: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

// --------------------------------------------------------------------------

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::from_conn(conn)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Store { conn: Mutex::new(conn) };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }

        // schema.sql is `CREATE TABLE IF NOT EXISTS` throughout, so on an
        // existing database it creates whatever is missing and is a no-op for
        // everything else. That makes it the right tool for a fresh install and
        // the wrong one for evolving a populated database: adding a column to a
        // table that already exists does nothing at all, and every later
        // insert then fails with "no such column".
        //
        // So each version gets an explicit step, applied in order and inside a
        // transaction, and `user_version` records how far a database has come.
        //
        // A database that does not exist yet is not at version 0 — it is at the
        // current version the moment `schema.sql` runs, because that file is
        // kept up to date. Running the steps over it too would try to add
        // columns it was just created with. `user_version` cannot tell the two
        // apart (both read 0), so the tables themselves are asked.
        let fresh: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
            [],
            |r| r.get(0),
        )?;
        let steps = if fresh == 0 { &MIGRATIONS[..0] } else { MIGRATIONS };
        apply_migrations(&conn, include_str!("schema.sql"), steps, version)?;
        // `apply_migrations` sets `user_version` to the number of steps it was
        // given, which for a fresh database is none of them.
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        if version > 0 {
            tracing::info!(from = version, to = SCHEMA_VERSION, "database schema migrated");
        }
        Ok(())
    }

    // ---------------------------- projects ----------------------------

    pub fn upsert_project(&self, id: &str, repo_url: &str, root_path: &str) -> Result<()> {
        let now = now_secs();
        self.conn.lock().execute(
            "INSERT INTO projects (id, repo_url, root_path, created_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET last_seen = ?4,
               repo_url = CASE WHEN excluded.repo_url != '' THEN excluded.repo_url ELSE repo_url END",
            params![id, repo_url, root_path, now],
        )?;
        Ok(())
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT * FROM projects ORDER BY last_seen DESC")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProjectRow {
                    id: r.get("id")?,
                    repo_url: r.get("repo_url")?,
                    root_path: r.get("root_path")?,
                    created_at: r.get("created_at")?,
                    last_seen: r.get("last_seen")?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn upsert_worktree(&self, id: &str, project_id: &str, label: &str) -> Result<()> {
        let now = now_secs();
        self.conn.lock().execute(
            "INSERT INTO worktrees (id, project_id, label, last_seen) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET last_seen = ?4, label = excluded.label",
            params![id, project_id, label, now],
        )?;
        Ok(())
    }

    pub fn list_worktrees(&self, project_id: Option<&str>) -> Result<Vec<WorktreeRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT * FROM worktrees WHERE (?1 IS NULL OR project_id = ?1) ORDER BY last_seen DESC LIMIT 500",
        )?;
        let rows = stmt
            .query_map(params![project_id], |r| {
                Ok(WorktreeRow {
                    id: r.get("id")?,
                    project_id: r.get("project_id")?,
                    label: r.get("label")?,
                    last_seen: r.get("last_seen")?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------ tasks ------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn insert_task(
        &self,
        row: &TaskRow,
        manifest_json: &str,
        profile_json: &str,
        base_commit: &str,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO tasks (id, task_type, project_id, worktree_id, agent_session, fingerprint,
                supersede_key, status, result_kind, command, image, log_ref, worker_id, attempt,
                created_at, started_at, finished_at, error, superseded_by, result_json,
                queue_ms, sync_ms, build_ms, bytes_synced, cache_hit)
             -- `image` is filled in by set_task_image once the task is placed;
             -- `command` must be bound here or the worker is handed an empty
             -- script and every task exits 0 having compiled nothing.
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'',?9, '','','',0, ?10,0,0,'','','',0,0,0,?11,0)",
            params![
                row.id,
                row.task_type,
                row.project_id,
                row.worktree_id,
                row.agent_session,
                row.fingerprint,
                row.supersede_key,
                row.status,
                row.command,
                row.created_at,
                row.bytes_synced,
            ],
        )?;
        conn.execute(
            "INSERT INTO task_inputs (task_id, manifest_json, profile_json, base_commit)
             VALUES (?1, ?2, ?3, ?4)",
            params![row.id, manifest_json, profile_json, base_commit],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> Result<Option<TaskRow>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row("SELECT * FROM tasks WHERE id = ?1", params![id], TaskRow::from_row)
            .optional()?;
        Ok(row)
    }

    pub fn get_task_inputs(&self, id: &str) -> Result<Option<(String, String, String)>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT manifest_json, profile_json, base_commit FROM task_inputs WHERE task_id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Task-level cache (§5.1). Only terminal, cacheable results within the
    /// TTL qualify; infra failures and timeouts must never be replayed.
    pub fn find_cached_result(&self, fingerprint: &str, ttl_secs: i64) -> Result<Option<TaskRow>> {
        let cutoff = now_ms() - ttl_secs * 1000;
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT * FROM tasks
                 WHERE fingerprint = ?1 AND status = 'done'
                   AND result_kind IN ('success','compile_error')
                   AND finished_at > ?2
                 ORDER BY finished_at DESC LIMIT 1",
                params![fingerprint, cutoff],
                TaskRow::from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// An in-flight task with the same fingerprint: subscribe instead of
    /// enqueuing a duplicate (§5.3).
    pub fn find_active_by_fingerprint(&self, fingerprint: &str) -> Result<Option<TaskRow>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT * FROM tasks
                 WHERE fingerprint = ?1
                   AND status IN ('pending','syncing','queued','running','uploading')
                 ORDER BY created_at ASC LIMIT 1",
                params![fingerprint],
                TaskRow::from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Not-yet-started tasks in the same supersede scope.
    pub fn find_supersede_candidates(&self, key: &str, exclude: &str) -> Result<Vec<TaskRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT * FROM tasks
             WHERE supersede_key = ?1 AND id != ?2
               AND status IN ('pending','syncing','queued')",
        )?;
        let rows = stmt
            .query_map(params![key, exclude], TaskRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Sessions other than `owner` that are waiting on this task. If any
    /// exist, the task must survive supersede (risk #23) or they hang forever.
    pub fn foreign_subscribers(&self, task_id: &str, owner: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT agent_session FROM task_subs WHERE task_id = ?1 AND agent_session != ?2")?;
        let rows = stmt
            .query_map(params![task_id, owner], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn add_subscriber(&self, task_id: &str, agent_session: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO task_subs (task_id, agent_session, at) VALUES (?1, ?2, ?3)",
            params![task_id, agent_session, now_secs()],
        )?;
        Ok(())
    }

    /// Detach a task from its supersede scope without cancelling it — used
    /// when foreign subscribers keep it alive (§5.2).
    pub fn detach_supersede_key(&self, task_id: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE tasks SET supersede_key = '' WHERE id = ?1",
            params![task_id],
        )?;
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE tasks SET status = ?2,
                 started_at = CASE WHEN ?2 = 'running' AND started_at = 0 THEN ?3 ELSE started_at END
             WHERE id = ?1",
            params![id, status, now_ms()],
        )?;
        Ok(())
    }

    pub fn mark_superseded(&self, id: &str, by: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE tasks SET status = 'superseded', superseded_by = ?2, finished_at = ?3 WHERE id = ?1",
            params![id, by, now_ms()],
        )?;
        Ok(())
    }

    pub fn assign_to_worker(&self, id: &str, worker_id: &str) -> Result<()> {
        let now = now_ms();
        self.conn.lock().execute(
            "UPDATE tasks SET worker_id = ?2, status = 'running', attempt = attempt + 1,
                 started_at = CASE WHEN started_at = 0 THEN ?3 ELSE started_at END,
                 queue_ms = CASE WHEN queue_ms = 0 THEN ?3 - created_at ELSE queue_ms END
             WHERE id = ?1",
            params![id, worker_id, now],
        )?;
        Ok(())
    }

    /// Return a task to the queue after an infra failure, remembering which
    /// workers already tried (they must not be picked again — §6.2).
    pub fn requeue(&self, id: &str, failed_worker: &str, reason: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO task_attempts (task_id, worker_id, at, error) VALUES (?1,?2,?3,?4)",
            params![id, failed_worker, now_ms(), reason],
        )?;
        conn.execute(
            "UPDATE tasks SET status = 'queued', worker_id = '' WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn attempted_workers(&self, task_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT worker_id FROM task_attempts WHERE task_id = ?1")?;
        let rows = stmt
            .query_map(params![task_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn attempt_records(&self, task_id: &str) -> Result<Vec<(String, i64, String)>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT worker_id, at, error FROM task_attempts WHERE task_id = ?1 ORDER BY at")?;
        let rows = stmt
            .query_map(params![task_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn complete_task(
        &self,
        id: &str,
        status: &str,
        result: &rc_core::pb::TaskResult,
        log_ref: &str,
        image: &str,
    ) -> Result<()> {
        let stats = result.stats.unwrap_or_default();
        let json = serde_json::to_string(result)?;
        self.conn.lock().execute(
            "UPDATE tasks SET status = ?2, result_kind = ?3, result_json = ?4, log_ref = ?5,
                 finished_at = ?6, sync_ms = ?7, build_ms = ?8, bytes_synced = ?9,
                 image = CASE WHEN ?10 != '' THEN ?10 ELSE image END
             WHERE id = ?1",
            params![
                id,
                status,
                result.kind,
                json,
                log_ref,
                now_ms(),
                stats.sync_ms as i64,
                stats.build_ms as i64,
                stats.bytes_synced as i64,
                image,
            ],
        )?;
        Ok(())
    }

    /// Serve a cached result under a fresh task id so the agent still gets a
    /// task handle it can poll and the admin UI shows the hit.
    pub fn record_cache_hit(&self, id: &str, source: &TaskRow) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE tasks SET status = 'done', result_kind = ?2, result_json = ?3, log_ref = ?4,
                 finished_at = ?5, cache_hit = 1, image = ?6
             WHERE id = ?1",
            params![
                id,
                source.result_kind,
                source.result_json,
                source.log_ref,
                now_ms(),
                source.image
            ],
        )?;
        Ok(())
    }

    pub fn set_image(&self, id: &str, image: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("UPDATE tasks SET image = ?2 WHERE id = ?1", params![id, image])?;
        Ok(())
    }

    pub fn add_timeline(&self, task_id: &str, phase: &str, worker_id: &str, detail: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO task_events (task_id, phase, at_ms, worker_id, detail) VALUES (?1,?2,?3,?4,?5)",
            params![task_id, phase, now_ms(), worker_id, detail],
        )?;
        Ok(())
    }

    pub fn timeline(&self, task_id: &str) -> Result<Vec<rc_core::pb::TaskPhase>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT phase, at_ms, worker_id, detail FROM task_events WHERE task_id = ?1 ORDER BY at_ms, rowid",
        )?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(rc_core::pb::TaskPhase {
                    phase: r.get(0)?,
                    at_ms: r.get(1)?,
                    worker_id: r.get(2)?,
                    detail: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Queued work in FIFO order, oldest first.
    pub fn queued_tasks(&self) -> Result<Vec<TaskRow>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT * FROM tasks WHERE status = 'queued' ORDER BY created_at ASC LIMIT 200")?;
        let rows = stmt
            .query_map([], TaskRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn tasks_on_worker(&self, worker_id: &str) -> Result<Vec<TaskRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT * FROM tasks WHERE worker_id = ?1 AND status IN ('running','uploading')",
        )?;
        let rows = stmt
            .query_map(params![worker_id], TaskRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Worktrees already executing somewhere — used to keep tasks for one
    /// worktree serialized on a worker (§6.2).
    pub fn busy_worktrees_on_worker(&self, worker_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT worktree_id FROM tasks WHERE worker_id = ?1 AND status IN ('running','uploading')",
        )?;
        let rows = stmt
            .query_map(params![worker_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_tasks(&self, f: &TaskFilter) -> Result<Vec<TaskRow>> {
        let limit = f.limit.unwrap_or(50).min(500) as i64;
        let offset = f.offset.unwrap_or(0) as i64;
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT * FROM tasks
             WHERE (?1 IS NULL OR project_id = ?1)
               AND (?2 IS NULL OR worktree_id = ?2)
               AND (?3 IS NULL OR agent_session = ?3)
               AND (?4 IS NULL OR status = ?4)
               AND (?5 IS NULL OR result_kind = ?5)
               AND (?6 IS NULL OR task_type = ?6)
             ORDER BY created_at DESC LIMIT ?7 OFFSET ?8",
        )?;
        let rows = stmt
            .query_map(
                params![
                    f.project_id,
                    f.worktree_id,
                    f.agent_session,
                    f.status,
                    f.result_kind,
                    f.task_type,
                    limit,
                    offset
                ],
                TaskRow::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn count_tasks(&self, f: &TaskFilter) -> Result<i64> {
        let conn = self.conn.lock();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE (?1 IS NULL OR project_id = ?1)
               AND (?2 IS NULL OR worktree_id = ?2)
               AND (?3 IS NULL OR agent_session = ?3)
               AND (?4 IS NULL OR status = ?4)
               AND (?5 IS NULL OR result_kind = ?5)
               AND (?6 IS NULL OR task_type = ?6)",
            params![f.project_id, f.worktree_id, f.agent_session, f.status, f.result_kind, f.task_type],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// After a control-plane restart, anything that was mid-flight has no
    /// owner: put it back in the queue (§5.3, risk #14).
    pub fn reset_inflight_on_boot(&self) -> Result<usize> {
        let n = self.conn.lock().execute(
            "UPDATE tasks SET status = 'queued', worker_id = ''
             WHERE status IN ('running','uploading','syncing')",
            [],
        )?;
        Ok(n)
    }

    /// Reap pending tasks nobody ever collected (§5.3). Disconnection alone
    /// never cancels a task — only this TTL does.
    pub fn expire_pending(&self, ttl_secs: i64) -> Result<Vec<String>> {
        let cutoff = now_ms() - ttl_secs * 1000;
        let conn = self.conn.lock();
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM tasks WHERE status IN ('pending','syncing','queued') AND created_at < ?1",
            )?;
            let out = stmt
                .query_map(params![cutoff], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out
        };
        for id in &ids {
            conn.execute(
                "UPDATE tasks SET status = 'canceled', error = 'pending ttl expired', finished_at = ?2 WHERE id = ?1",
                params![id, now_ms()],
            )?;
        }
        Ok(ids)
    }

    // ----------------------------- profiles -----------------------------

    pub fn get_profile(&self, project_id: &str, path: &str) -> Result<Option<ProfileRow>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT * FROM profiles WHERE project_id = ?1 AND path = ?2",
                params![project_id, path],
                profile_from_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn upsert_profile(&self, p: &ProfileRow) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO profiles (id, project_id, path, adapter, image, config_toml, created_by,
                 last_success_at, success_count, total_count, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,0,0,0,?8)
             ON CONFLICT(project_id, path) DO UPDATE SET
                 adapter = excluded.adapter, image = excluded.image,
                 config_toml = excluded.config_toml, updated_at = excluded.updated_at",
            params![
                p.id,
                p.project_id,
                p.path,
                p.adapter,
                p.image,
                p.config_toml,
                p.created_by,
                now_secs()
            ],
        )?;
        Ok(())
    }

    pub fn record_profile_outcome(&self, project_id: &str, path: &str, success: bool) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE profiles SET total_count = total_count + 1,
                 success_count = success_count + ?3,
                 last_success_at = CASE WHEN ?3 = 1 THEN ?4 ELSE last_success_at END
             WHERE project_id = ?1 AND path = ?2",
            params![project_id, path, i64::from(success), now_secs()],
        )?;
        Ok(())
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT * FROM profiles ORDER BY updated_at DESC LIMIT 500")?;
        let rows = stmt
            .query_map([], profile_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_profile(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM profiles WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ------------------------------ images ------------------------------

    pub fn upsert_image(&self, img: &ImageRow) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO images (id, image_ref, digest, dockerfile, pull_ref, status, arch, targets,
                 description, created_by, approved_by, approved_at, last_success_at, success_count,
                 total_count, consecutive_env_errors, built_at, created_at, build_log_ref, message)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'',0,0,0,0,0,0,?11,'','')
             ON CONFLICT(id) DO UPDATE SET
                 image_ref = excluded.image_ref, digest = excluded.digest,
                 status = excluded.status, description = excluded.description",
            params![
                img.id,
                img.image_ref,
                img.digest,
                img.dockerfile,
                img.pull_ref,
                img.status,
                img.arch,
                img.targets,
                img.description,
                img.created_by,
                now_secs(),
            ],
        )?;
        Ok(())
    }

    pub fn get_image(&self, id: &str) -> Result<Option<ImageRow>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT * FROM images WHERE id = ?1", params![id], image_from_row)
            .optional()?)
    }

    pub fn list_images(&self, status: Option<&str>) -> Result<Vec<ImageRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT * FROM images WHERE (?1 IS NULL OR status = ?1) ORDER BY created_at DESC LIMIT 500",
        )?;
        let rows = stmt
            .query_map(params![status], image_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_image_status(&self, id: &str, status: &str, message: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE images SET status = ?2, message = ?3 WHERE id = ?1",
            params![id, status, message],
        )?;
        Ok(())
    }

    pub fn approve_image(&self, id: &str, admin: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE images SET approved_by = ?2, approved_at = ?3,
                 status = CASE WHEN digest != '' THEN 'healthy' ELSE 'building' END
             WHERE id = ?1",
            params![id, admin, now_secs()],
        )?;
        Ok(())
    }

    pub fn finish_image_build(&self, id: &str, digest: &str, log_ref: &str, ok: bool, message: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE images SET digest = ?2, build_log_ref = ?3, built_at = ?4,
                 status = CASE WHEN ?5 = 1 THEN 'healthy' ELSE 'failing' END,
                 message = ?6
             WHERE id = ?1",
            params![id, digest, log_ref, now_secs(), i64::from(ok), message],
        )?;
        Ok(())
    }

    /// Feed a task outcome back into image health (§8.5).
    /// Fold one task outcome into an image's health (§8.5).
    ///
    /// `env_error` is a much weaker signal about an *image* than it looks. The
    /// common case is a project needing a native library the image was never
    /// asked to carry: `rrd-sys` wanting `librrd` says nothing about whether
    /// the image builds anything else. Charging that to the image took the
    /// fleet's only image out of rotation after three checks of one repository,
    /// and nothing put it back, because the status only ever moved one way.
    ///
    /// So an `env_error` counts against the image only when it is not obviously
    /// the project's own to fix:
    ///
    /// * `named_missing_dep` — the log named a library or program to install.
    ///   That is a statement about the project's needs, never about the image.
    /// * the same project failing again. One repository retrying its own
    ///   missing dependency is one fact, not three.
    ///
    /// Consecutive failures across *different* projects are what suggest the
    /// image itself is broken. And a success now restores `failing` to
    /// `healthy`: whatever was wrong evidently is not any more.
    pub fn record_image_outcome(
        &self,
        digest: &str,
        kind: &str,
        project_id: &str,
        named_missing_dep: bool,
    ) -> Result<()> {
        if digest.is_empty() {
            return Ok(());
        }
        let success = kind == "success" || kind == "compile_error";
        let counts_against_image = kind == "env_error" && !named_missing_dep;
        self.conn.lock().execute(
            "UPDATE images SET
                 total_count = total_count + 1,
                 success_count = success_count + ?2,
                 last_success_at = CASE WHEN ?2 = 1 THEN ?5 ELSE last_success_at END,
                 consecutive_env_errors = CASE
                     WHEN ?2 = 1 THEN 0
                     WHEN ?3 = 1 AND last_env_error_project != ?4 THEN consecutive_env_errors + 1
                     ELSE consecutive_env_errors END,
                 last_env_error_project = CASE
                     WHEN ?2 = 1 THEN ''
                     WHEN ?3 = 1 THEN ?4
                     ELSE last_env_error_project END,
                 status = CASE
                     WHEN ?2 = 1 AND status = 'failing' THEN 'healthy'
                     WHEN ?3 = 1 AND last_env_error_project != ?4
                          AND consecutive_env_errors + 1 >= 3 AND status = 'healthy'
                     THEN 'failing'
                     ELSE status END
             WHERE digest = ?1",
            params![
                digest,
                i64::from(success),
                i64::from(counts_against_image),
                project_id,
                now_secs()
            ],
        )?;
        Ok(())
    }

    /// Images approved by an admin — only these may execute untrusted code
    /// (§8.3).
    pub fn is_digest_trusted(&self, digest: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM images WHERE digest = ?1 AND approved_by != '' AND status != 'rejected'",
            params![digest],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn image_usage(&self, digest: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT project_id FROM tasks WHERE image = ?1 AND project_id != '' LIMIT 50",
        )?;
        let rows = stmt
            .query_map(params![digest], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------ workers ------------------------------

    pub fn upsert_worker(&self, w: &WorkerRow, token_hash: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO workers (id, arch, labels, capacity, status, version, max_parallel,
                 enrolled_at, last_hb, token_hash)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?9)
             ON CONFLICT(id) DO UPDATE SET arch = excluded.arch, labels = excluded.labels,
                 capacity = excluded.capacity, status = excluded.status,
                 version = excluded.version, max_parallel = excluded.max_parallel,
                 token_hash = excluded.token_hash",
            params![
                w.id,
                w.arch,
                w.labels,
                w.capacity,
                w.status,
                w.version,
                w.max_parallel,
                now_secs(),
                token_hash
            ],
        )?;
        Ok(())
    }

    pub fn worker_token_hash(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT token_hash FROM workers WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?)
    }

    pub fn touch_worker(&self, id: &str, status: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE workers SET last_hb = ?2, status = ?3 WHERE id = ?1",
            params![id, now_secs(), status],
        )?;
        Ok(())
    }

    pub fn set_worker_status(&self, id: &str, status: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("UPDATE workers SET status = ?2 WHERE id = ?1", params![id, status])?;
        Ok(())
    }

    pub fn list_workers(&self) -> Result<Vec<WorkerRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT * FROM workers ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(WorkerRow {
                    id: r.get("id")?,
                    arch: r.get("arch")?,
                    labels: r.get("labels")?,
                    capacity: r.get("capacity")?,
                    status: r.get("status")?,
                    version: r.get("version")?,
                    max_parallel: r.get("max_parallel")?,
                    enrolled_at: r.get("enrolled_at")?,
                    last_hb: r.get("last_hb")?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_worker(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM workers WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Workers whose heartbeat has gone quiet (§9).
    pub fn stale_workers(&self, older_than_secs: i64) -> Result<Vec<String>> {
        let cutoff = now_secs() - older_than_secs;
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT id FROM workers WHERE last_hb < ?1 AND status != 'offline'")?;
        let rows = stmt
            .query_map(params![cutoff], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------- CAS -------------------------------

    /// Record a blob and bump its lease. Reconciliation *is* renewal (§4.7).
    pub fn touch_blobs(&self, hashes: &[(String, i64)]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for (hash, size) in hashes {
            tx.execute(
                "INSERT INTO cas_blobs (hash, size, last_used, pinned) VALUES (?1, ?2, ?3, 0)
                 ON CONFLICT(hash) DO UPDATE SET last_used = ?3,
                   size = CASE WHEN excluded.size > 0 THEN excluded.size ELSE size END",
                params![hash, size, now_secs()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Pin every blob a task references until the task reaches a terminal
    /// state, so GC cannot pull the rug out mid-build (§4.7).
    pub fn pin_task_blobs(&self, task_id: &str, hashes: &[String]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for h in hashes {
            tx.execute(
                "INSERT OR IGNORE INTO task_blob_refs (task_id, hash) VALUES (?1, ?2)",
                params![task_id, h],
            )?;
            // Upsert rather than update: a task must never reference a blob
            // that has no accounting row, or GC would lose track of it.
            tx.execute(
                "INSERT INTO cas_blobs (hash, size, last_used, pinned) VALUES (?1, 0, ?2, 1)
                 ON CONFLICT(hash) DO UPDATE SET pinned = pinned + 1, last_used = ?2",
                params![h, now_secs()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn unpin_task_blobs(&self, task_id: &str) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let hashes: Vec<String> = {
                let mut stmt = tx.prepare("SELECT hash FROM task_blob_refs WHERE task_id = ?1")?;
                let out = stmt
                    .query_map(params![task_id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                out
            };
            for h in hashes {
                tx.execute(
                    "UPDATE cas_blobs SET pinned = MAX(0, pinned - 1) WHERE hash = ?1",
                    params![h],
                )?;
            }
            tx.execute("DELETE FROM task_blob_refs WHERE task_id = ?1", params![task_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// GC candidates: unpinned, unreferenced and cold (§9).
    pub fn collectable_blobs(&self, ttl_secs: i64, limit: i64) -> Result<Vec<BlobRow>> {
        let cutoff = now_secs() - ttl_secs;
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT b.hash, b.size, b.last_used, b.pinned FROM cas_blobs b
             WHERE b.pinned = 0 AND b.last_used < ?1
               AND NOT EXISTS (SELECT 1 FROM task_blob_refs r WHERE r.hash = b.hash)
             ORDER BY b.last_used ASC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![cutoff, limit], |r| {
                Ok(BlobRow {
                    hash: r.get(0)?,
                    size: r.get(1)?,
                    last_used: r.get(2)?,
                    pinned: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn forget_blob(&self, hash: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM cas_blobs WHERE hash = ?1", params![hash])?;
        Ok(())
    }

    pub fn cas_summary(&self) -> Result<(i64, i64, i64)> {
        let conn = self.conn.lock();
        let (count, bytes): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size),0) FROM cas_blobs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let pinned: i64 =
            conn.query_row("SELECT COUNT(*) FROM cas_blobs WHERE pinned > 0", [], |r| r.get(0))?;
        Ok((count, bytes, pinned))
    }

    // --------------------------- git baselines ---------------------------

    pub fn note_known_commit(&self, project_id: &str, commit: &str) -> Result<()> {
        if commit.is_empty() {
            return Ok(());
        }
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO project_commits (project_id, commit_sha, at) VALUES (?1,?2,?3)",
            params![project_id, commit, now_secs()],
        )?;
        Ok(())
    }

    pub fn has_commit(&self, project_id: &str, commit: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM project_commits WHERE project_id = ?1 AND commit_sha = ?2",
            params![project_id, commit],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Commits the fleet can already reach — bundle base points (§4.1 step 3).
    pub fn known_commits(&self, project_id: &str, limit: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT commit_sha FROM project_commits WHERE project_id = ?1 ORDER BY at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![project_id, limit], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn add_bundle(&self, project_id: &str, commit: &str, blob_hash: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO project_bundles (project_id, commit_sha, blob_hash, at)
             VALUES (?1,?2,?3,?4)",
            params![project_id, commit, blob_hash, now_secs()],
        )?;
        Ok(())
    }

    /// Every bundle a worker needs to reconstruct `commit`, oldest first.
    pub fn bundles_for(&self, project_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT blob_hash FROM project_bundles WHERE project_id = ?1 ORDER BY at ASC")?;
        let rows = stmt
            .query_map(params![project_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------- auth -------------------------------

    pub fn create_admin(&self, username: &str, password_hash: &str, role: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO admins (username, password_hash, role, created_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(username) DO UPDATE SET password_hash = excluded.password_hash,
                 role = excluded.role",
            params![username, password_hash, role, now_secs()],
        )?;
        Ok(())
    }

    pub fn get_admin(&self, username: &str) -> Result<Option<(String, String)>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT password_hash, role FROM admins WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn list_admins(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT username, role, created_at FROM admins ORDER BY username")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_admin(&self, username: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM admins WHERE username = ?1", params![username])?;
        Ok(())
    }

    pub fn admin_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM admins", [], |r| r.get(0))?)
    }

    pub fn create_session(&self, token: &str, username: &str, role: &str, ttl_secs: i64) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO sessions (token, username, role, created_at, expires_at) VALUES (?1,?2,?3,?4,?5)",
            params![token, username, role, now_secs(), now_secs() + ttl_secs],
        )?;
        Ok(())
    }

    pub fn lookup_session(&self, token: &str) -> Result<Option<(String, String)>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT username, role FROM sessions WHERE token = ?1 AND expires_at > ?2",
                params![token, now_secs()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
        Ok(())
    }

    pub fn add_agent_token(&self, token_hash: &str, label: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO agent_tokens (token_hash, label, created_at, last_used) VALUES (?1,?2,?3,0)",
            params![token_hash, label, now_secs()],
        )?;
        Ok(())
    }

    pub fn agent_token_valid(&self, token_hash: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_tokens WHERE token_hash = ?1",
            params![token_hash],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn list_agent_tokens(&self) -> Result<Vec<(String, String, i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT token_hash, label, created_at, last_used FROM agent_tokens ORDER BY created_at DESC")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_agent_token(&self, token_hash: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM agent_tokens WHERE token_hash = ?1", params![token_hash])?;
        Ok(())
    }

    /// Enrollment tokens are single-use and time limited (§8.1).
    pub fn add_enrollment_token(&self, token: &str, created_by: &str, ttl_secs: i64) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO enrollment_tokens (token, created_by, created_at, expires_at, used_at, used_by)
             VALUES (?1,?2,?3,?4,0,'')",
            params![token, created_by, now_secs(), now_secs() + ttl_secs],
        )?;
        Ok(())
    }

    pub fn consume_enrollment_token(&self, token: &str, worker_id: &str) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE enrollment_tokens SET used_at = ?2, used_by = ?3
             WHERE token = ?1 AND used_at = 0 AND expires_at > ?2",
            params![token, now_secs(), worker_id],
        )?;
        Ok(n == 1)
    }

    pub fn list_enrollment_tokens(&self) -> Result<Vec<EnrollmentTokenRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT token, created_by, created_at, expires_at, used_at, used_by
             FROM enrollment_tokens ORDER BY created_at DESC LIMIT 100",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(EnrollmentTokenRow {
                    token: r.get(0)?,
                    created_by: r.get(1)?,
                    created_at: r.get(2)?,
                    expires_at: r.get(3)?,
                    used_at: r.get(4)?,
                    used_by: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------ audit ------------------------------

    pub fn audit(&self, actor: &str, action: &str, target: &str, detail: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO audit_log (at, actor, action, target, detail) VALUES (?1,?2,?3,?4,?5)",
            params![now_secs(), actor, action, target, detail],
        )?;
        Ok(())
    }

    pub fn list_audit(&self, limit: i64) -> Result<Vec<AuditRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT id, at, actor, action, target, detail FROM audit_log ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok(AuditRow {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    actor: r.get(2)?,
                    action: r.get(3)?,
                    target: r.get(4)?,
                    detail: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------ alerts ------------------------------

    pub fn raise_alert(&self, rule: &str, level: &str, message: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let open: i64 = conn.query_row(
            "SELECT COUNT(*) FROM alerts WHERE rule = ?1 AND resolved_at = 0",
            params![rule],
            |r| r.get(0),
        )?;
        if open > 0 {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO alerts (rule, level, message, at, resolved_at) VALUES (?1,?2,?3,?4,0)",
            params![rule, level, message, now_secs()],
        )?;
        Ok(true)
    }

    pub fn resolve_alert(&self, rule: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE alerts SET resolved_at = ?2 WHERE rule = ?1 AND resolved_at = 0",
            params![rule, now_secs()],
        )?;
        Ok(())
    }

    pub fn list_alerts(&self, include_resolved: bool) -> Result<Vec<AlertRow>> {
        let conn = self.conn.lock();
        let sql = if include_resolved {
            "SELECT id, rule, level, message, at, resolved_at FROM alerts ORDER BY id DESC LIMIT 200"
        } else {
            "SELECT id, rule, level, message, at, resolved_at FROM alerts WHERE resolved_at = 0 ORDER BY id DESC LIMIT 200"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AlertRow {
                    id: r.get(0)?,
                    rule: r.get(1)?,
                    level: r.get(2)?,
                    message: r.get(3)?,
                    at: r.get(4)?,
                    resolved_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ----------------------------- settings -----------------------------

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO settings (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT v FROM settings WHERE k = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn all_settings(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT k, v FROM settings ORDER BY k")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ----------------------------- metrics -----------------------------

    /// Batched rollup insert (§15.1): one transaction per flush, never one
    /// write per sample.
    pub fn write_rollup(&self, granularity: &str, points: &[(String, i64, f64, i64)]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for (metric, bucket, sum, count) in points {
            tx.execute(
                "INSERT INTO metrics_rollup (metric, granularity, bucket_ts, sum, count)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(metric, granularity, bucket_ts) DO UPDATE SET
                     sum = sum + excluded.sum, count = count + excluded.count",
                params![metric, granularity, bucket, sum, count],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn read_series(
        &self,
        metric: &str,
        granularity: &str,
        since: i64,
    ) -> Result<Vec<(i64, f64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT bucket_ts, sum, count FROM metrics_rollup
             WHERE metric = ?1 AND granularity = ?2 AND bucket_ts >= ?3
             ORDER BY bucket_ts",
        )?;
        let rows = stmt
            .query_map(params![metric, granularity, since], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn prune_rollups(&self, granularity: &str, older_than: i64) -> Result<usize> {
        Ok(self.conn.lock().execute(
            "DELETE FROM metrics_rollup WHERE granularity = ?1 AND bucket_ts < ?2",
            params![granularity, older_than],
        )?)
    }

    // --------------------------- aggregations ---------------------------

    /// Numbers behind the Overview cards.
    pub fn overview_counters(&self, window_secs: i64) -> Result<OverviewCounters> {
        let since = now_ms() - window_secs * 1000;
        let conn = self.conn.lock();
        let running: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE status IN ('running','uploading')",
            [],
            |r| r.get(0),
        )?;
        let queued: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE status IN ('pending','syncing','queued')",
            [],
            |r| r.get(0),
        )?;
        let (finished, success, cache_hits, superseded, infra, timeout): (i64, i64, i64, i64, i64, i64) =
            conn.query_row(
                "SELECT
                    COUNT(*),
                    COALESCE(SUM(result_kind = 'success'), 0),
                    COALESCE(SUM(cache_hit), 0),
                    COALESCE(SUM(status = 'superseded'), 0),
                    COALESCE(SUM(result_kind = 'infra_error'), 0),
                    COALESCE(SUM(result_kind = 'timeout'), 0)
                 FROM tasks WHERE created_at > ?1",
                params![since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
        let bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(bytes_synced),0) FROM tasks WHERE created_at > ?1",
            params![since],
            |r| r.get(0),
        )?;
        Ok(OverviewCounters {
            running,
            queued,
            finished_window: finished,
            success_window: success,
            cache_hits_window: cache_hits,
            superseded_window: superseded,
            infra_errors_window: infra,
            timeouts_window: timeout,
            bytes_synced_window: bytes,
        })
    }

    /// Phase-duration percentiles for the monitoring page.
    pub fn phase_percentiles(&self, window_secs: i64) -> Result<Vec<(String, f64, f64)>> {
        let since = now_ms() - window_secs * 1000;
        let conn = self.conn.lock();
        let mut out = Vec::new();
        for (label, col) in [("queue", "queue_ms"), ("sync", "sync_ms"), ("build", "build_ms")] {
            let sql = format!(
                "SELECT {col} FROM tasks WHERE created_at > ?1 AND {col} > 0 ORDER BY {col}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let vals: Vec<f64> = stmt
                .query_map(params![since], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|v| v as f64)
                .collect();
            out.push((label.to_string(), percentile(&vals, 0.5), percentile(&vals, 0.95)));
        }
        Ok(out)
    }

    /// Success-rate buckets for the trend chart.
    pub fn task_histogram(&self, bucket_secs: i64, buckets: i64) -> Result<Vec<(i64, i64, i64, i64)>> {
        let now = now_secs();
        let start = (now - bucket_secs * buckets) * 1000;
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT (created_at / 1000 / ?1) * ?1 AS bucket,
                    COUNT(*),
                    COALESCE(SUM(result_kind = 'success'),0),
                    COALESCE(SUM(cache_hit),0)
             FROM tasks WHERE created_at >= ?2
             GROUP BY bucket ORDER BY bucket",
        )?;
        let rows = stmt
            .query_map(params![bucket_secs, start], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct OverviewCounters {
    pub running: i64,
    pub queued: i64,
    pub finished_window: i64,
    pub success_window: i64,
    pub cache_hits_window: i64,
    pub superseded_window: i64,
    pub infra_errors_window: i64,
    pub timeouts_window: i64,
    pub bytes_synced_window: i64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn profile_from_row(r: &Row<'_>) -> rusqlite::Result<ProfileRow> {
    Ok(ProfileRow {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        path: r.get("path")?,
        adapter: r.get("adapter")?,
        image: r.get("image")?,
        config_toml: r.get("config_toml")?,
        created_by: r.get("created_by")?,
        last_success_at: r.get("last_success_at")?,
        success_count: r.get("success_count")?,
        total_count: r.get("total_count")?,
        updated_at: r.get("updated_at")?,
    })
}

fn image_from_row(r: &Row<'_>) -> rusqlite::Result<ImageRow> {
    Ok(ImageRow {
        id: r.get("id")?,
        image_ref: r.get("image_ref")?,
        digest: r.get("digest")?,
        dockerfile: r.get("dockerfile")?,
        pull_ref: r.get("pull_ref")?,
        status: r.get("status")?,
        arch: r.get("arch")?,
        targets: r.get("targets")?,
        description: r.get("description")?,
        created_by: r.get("created_by")?,
        approved_by: r.get("approved_by")?,
        approved_at: r.get("approved_at")?,
        last_success_at: r.get("last_success_at")?,
        success_count: r.get("success_count")?,
        total_count: r.get("total_count")?,
        consecutive_env_errors: r.get("consecutive_env_errors")?,
        built_at: r.get("built_at")?,
        created_at: r.get("created_at")?,
        build_log_ref: r.get("build_log_ref")?,
        message: r.get("message")?,
    })
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use rc_core::pb::TaskResult;

    fn task(id: &str, fp: &str, key: &str, session: &str, status: &str) -> TaskRow {
        TaskRow {
            id: id.into(),
            task_type: "check".into(),
            project_id: "p1".into(),
            worktree_id: "w1".into(),
            agent_session: session.into(),
            fingerprint: fp.into(),
            supersede_key: key.into(),
            status: status.into(),
            created_at: now_ms(),
            ..Default::default()
        }
    }

    fn store() -> Store {
        Store::open_memory().unwrap()
    }

    #[test]
    fn schema_applies_and_is_idempotent() {
        let s = store();
        s.migrate().unwrap();
        assert_eq!(s.admin_count().unwrap(), 0);
    }

    #[test]
    fn task_insert_and_lookup() {
        let s = store();
        s.insert_task(&task("t1", "fp1", "k1", "s1", "queued"), "{}", "{}", "abc").unwrap();
        let got = s.get_task("t1").unwrap().unwrap();
        assert_eq!(got.fingerprint, "fp1");
        assert_eq!(s.get_task_inputs("t1").unwrap().unwrap().2, "abc");
    }

    #[test]
    fn task_insert_keeps_the_command() {
        // Risk: the worker runs whatever this column holds. Losing it means an
        // empty script, exit 0, and a green verdict for a build that never ran.
        let s = store();
        let mut row = task("t1", "fp1", "k1", "s1", "queued");
        row.command = "cargo check --workspace --all-targets".into();
        s.insert_task(&row, "{}", "{}", "abc").unwrap();

        let got = s.get_task("t1").unwrap().unwrap();
        assert_eq!(got.command, "cargo check --workspace --all-targets");
        assert_eq!(got.image, "", "the image is only known once the task is placed");
    }

    #[test]
    fn cache_lookup_ignores_infra_errors_and_expired_rows() {
        let s = store();
        s.insert_task(&task("t1", "fp", "k", "s", "queued"), "{}", "{}", "").unwrap();
        s.complete_task("t1", "done", &TaskResult { kind: "infra_error".into(), ..Default::default() }, "", "")
            .unwrap();
        // Risk: replaying an infra failure would tell the agent its code is fine.
        assert!(s.find_cached_result("fp", 3600).unwrap().is_none());

        s.insert_task(&task("t2", "fp", "k", "s", "queued"), "{}", "{}", "").unwrap();
        s.complete_task("t2", "done", &TaskResult { kind: "success".into(), ..Default::default() }, "", "")
            .unwrap();
        assert!(s.find_cached_result("fp", 3600).unwrap().is_some());
        // TTL boundary: a zero-second TTL must never hit.
        assert!(s.find_cached_result("fp", -1).unwrap().is_none());
    }

    #[test]
    fn supersede_candidates_respect_scope_and_progress() {
        let s = store();
        s.insert_task(&task("old", "f1", "w1|s1|check", "s1", "queued"), "{}", "{}", "").unwrap();
        s.insert_task(&task("running", "f2", "w1|s1|check", "s1", "running"), "{}", "{}", "").unwrap();
        s.insert_task(&task("other_type", "f3", "w1|s1|clippy", "s1", "queued"), "{}", "{}", "").unwrap();
        s.insert_task(&task("other_sess", "f4", "w1|s2|check", "s2", "queued"), "{}", "{}", "").unwrap();

        let c = s.find_supersede_candidates("w1|s1|check", "new").unwrap();
        let ids: Vec<&str> = c.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["old"]);
    }

    #[test]
    fn foreign_subscribers_are_visible() {
        let s = store();
        s.insert_task(&task("t1", "f", "k", "s1", "queued"), "{}", "{}", "").unwrap();
        s.add_subscriber("t1", "s1").unwrap();
        s.add_subscriber("t1", "s2").unwrap();
        assert_eq!(s.foreign_subscribers("t1", "s1").unwrap(), vec!["s2"]);
        assert!(s.foreign_subscribers("t1", "s2").unwrap().contains(&"s1".to_string()));
    }

    #[test]
    fn restart_requeues_inflight_work() {
        let s = store();
        s.insert_task(&task("t1", "f", "k", "s", "running"), "{}", "{}", "").unwrap();
        s.insert_task(&task("t2", "f2", "k", "s", "done"), "{}", "{}", "").unwrap();
        assert_eq!(s.reset_inflight_on_boot().unwrap(), 1);
        assert_eq!(s.get_task("t1").unwrap().unwrap().status, "queued");
        assert_eq!(s.get_task("t2").unwrap().unwrap().status, "done");
    }

    #[test]
    fn pinned_blobs_survive_gc_until_the_task_ends() {
        let s = store();
        let h = "a".repeat(64);
        s.touch_blobs(&[(h.clone(), 10)]).unwrap();
        s.insert_task(&task("t1", "f", "k", "s", "queued"), "{}", "{}", "").unwrap();
        s.pin_task_blobs("t1", std::slice::from_ref(&h)).unwrap();
        // Even long past the TTL, a pinned blob is not collectable (§4.7).
        assert!(s.collectable_blobs(-1, 10).unwrap().is_empty());
        s.unpin_task_blobs("t1").unwrap();
        assert_eq!(s.collectable_blobs(-1, 10).unwrap().len(), 1);
    }

    #[test]
    fn only_approved_digests_are_trusted() {
        let s = store();
        let img = ImageRow {
            id: "e1".into(),
            digest: "sha256:x".into(),
            status: "pending_approval".into(),
            ..Default::default()
        };
        s.upsert_image(&img).unwrap();
        assert!(!s.is_digest_trusted("sha256:x").unwrap());
        s.approve_image("e1", "admin").unwrap();
        assert!(s.is_digest_trusted("sha256:x").unwrap());
    }

    #[test]
    fn repeated_env_errors_mark_an_image_failing() {
        let s = store();
        s.upsert_image(&ImageRow {
            id: "e1".into(),
            digest: "sha256:y".into(),
            status: "healthy".into(),
            ..Default::default()
        })
        .unwrap();
        // Different projects failing in a row is what suggests the image.
        for p in ["p1", "p2", "p3"] {
            s.record_image_outcome("sha256:y", "env_error", p, false).unwrap();
        }
        assert_eq!(s.get_image("e1").unwrap().unwrap().status, "failing");
    }

    #[test]
    fn a_populated_v0_database_gains_the_new_column() {
        // The deployed database predates the migration mechanism carrying any
        // steps, so it reads as version 0 while already holding data. It has to
        // take the ALTER; a fresh one must not, having been created with the
        // column already.
        let dir = std::env::temp_dir().join(format!("rc-migrate-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rc.sqlite");
        {
            // The real schema, wound back to the shape the deployed database
            // actually has: the column dropped and the version reset.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(include_str!("schema.sql")).unwrap();
            conn.execute_batch(
                "ALTER TABLE images DROP COLUMN last_env_error_project;
                 INSERT INTO images (id, digest) VALUES ('e1', 'd');",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 0i64).unwrap();
        }

        let s = Store::open(&path).unwrap();
        // The pre-existing row survived, and the new column is usable.
        s.record_image_outcome("d", "env_error", "p1", false).unwrap();
        assert_eq!(s.get_image("e1").unwrap().unwrap().consecutive_env_errors, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_project_failing_repeatedly_does_not_condemn_the_image() {
        // The failure that motivated this: three checks of one repository whose
        // build needs a library the image never carried took the fleet's only
        // image out of rotation for every other project.
        let s = store();
        s.upsert_image(&ImageRow { id: "e1".into(), digest: "d".into(), status: "healthy".into(), ..Default::default() })
            .unwrap();
        for _ in 0..5 {
            s.record_image_outcome("d", "env_error", "p-zfc", false).unwrap();
        }
        let img = s.get_image("e1").unwrap().unwrap();
        assert_eq!(img.status, "healthy");
        assert_eq!(img.consecutive_env_errors, 1, "one project is one fact");
    }

    #[test]
    fn an_env_error_that_names_its_missing_library_is_never_the_images_fault() {
        // "install librrd-dev" is a statement about what the project needs. The
        // image is working exactly as built.
        let s = store();
        s.upsert_image(&ImageRow { id: "e1".into(), digest: "d".into(), status: "healthy".into(), ..Default::default() })
            .unwrap();
        for p in ["p1", "p2", "p3", "p4"] {
            s.record_image_outcome("d", "env_error", p, true).unwrap();
        }
        let img = s.get_image("e1").unwrap().unwrap();
        assert_eq!(img.status, "healthy");
        assert_eq!(img.consecutive_env_errors, 0);
        assert_eq!(img.total_count, 4, "still counted as a run");
    }

    #[test]
    fn a_success_clears_the_env_error_streak() {
        let s = store();
        s.upsert_image(&ImageRow { id: "e1".into(), digest: "d".into(), status: "healthy".into(), ..Default::default() })
            .unwrap();
        s.record_image_outcome("d", "env_error", "p1", false).unwrap();
        s.record_image_outcome("d", "success", "p1", false).unwrap();
        s.record_image_outcome("d", "env_error", "p2", false).unwrap();
        assert_eq!(s.get_image("e1").unwrap().unwrap().status, "healthy");
    }

    #[test]
    fn a_success_brings_a_failing_image_back() {
        // `failing` used to be a one-way door: the status only ever moved from
        // healthy, so an image condemned by a since-fixed problem stayed out of
        // rotation forever, even after builds started passing on it again.
        let s = store();
        s.upsert_image(&ImageRow { id: "e1".into(), digest: "d".into(), status: "healthy".into(), ..Default::default() })
            .unwrap();
        for p in ["p1", "p2", "p3"] {
            s.record_image_outcome("d", "env_error", p, false).unwrap();
        }
        assert_eq!(s.get_image("e1").unwrap().unwrap().status, "failing");

        s.record_image_outcome("d", "success", "p1", false).unwrap();
        let img = s.get_image("e1").unwrap().unwrap();
        assert_eq!(img.status, "healthy");
        assert_eq!(img.consecutive_env_errors, 0);
    }

    #[test]
    fn enrollment_tokens_are_single_use() {
        let s = store();
        s.add_enrollment_token("tok", "admin", 3600).unwrap();
        assert!(s.consume_enrollment_token("tok", "w1").unwrap());
        assert!(!s.consume_enrollment_token("tok", "w2").unwrap());
    }

    #[test]
    fn expired_enrollment_tokens_are_refused() {
        let s = store();
        s.add_enrollment_token("tok", "admin", -1).unwrap();
        assert!(!s.consume_enrollment_token("tok", "w1").unwrap());
    }

    #[test]
    fn alerts_do_not_duplicate_while_open() {
        let s = store();
        assert!(s.raise_alert("worker_offline", "warn", "w1 gone").unwrap());
        assert!(!s.raise_alert("worker_offline", "warn", "w1 gone").unwrap());
        s.resolve_alert("worker_offline").unwrap();
        assert!(s.raise_alert("worker_offline", "warn", "w1 gone again").unwrap());
    }

    #[test]
    fn rollups_accumulate_into_buckets() {
        let s = store();
        s.write_rollup("1min", &[("tasks".into(), 60, 2.0, 2)]).unwrap();
        s.write_rollup("1min", &[("tasks".into(), 60, 3.0, 1)]).unwrap();
        let series = s.read_series("tasks", "1min", 0).unwrap();
        assert_eq!(series, vec![(60, 5.0, 3)]);
    }

    #[test]
    fn overview_counts_running_and_queued() {
        let s = store();
        s.insert_task(&task("t1", "f1", "k", "s", "running"), "{}", "{}", "").unwrap();
        s.insert_task(&task("t2", "f2", "k", "s", "queued"), "{}", "{}", "").unwrap();
        let c = s.overview_counters(3600).unwrap();
        assert_eq!(c.running, 1);
        assert_eq!(c.queued, 1);
    }

    #[test]
    fn retry_never_reuses_a_failed_worker() {
        let s = store();
        s.insert_task(&task("t1", "f", "k", "s", "queued"), "{}", "{}", "").unwrap();
        s.assign_to_worker("t1", "w-a").unwrap();
        s.requeue("t1", "w-a", "docker daemon gone").unwrap();
        assert_eq!(s.attempted_workers("t1").unwrap(), vec!["w-a"]);
        assert_eq!(s.get_task("t1").unwrap().unwrap().status, "queued");
    }

    #[test]
    fn a_migration_step_alters_a_table_that_already_holds_rows() {
        // Re-running `CREATE TABLE IF NOT EXISTS` cannot add a column: on a
        // populated database it is a silent no-op, and every later query then
        // fails with "no such column". This is the mechanism that makes adding
        // one actually work.
        let conn = Connection::open_in_memory().unwrap();
        let base = "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, a TEXT NOT NULL DEFAULT '');";
        apply_migrations(&conn, base, &[""], 0).unwrap();
        conn.execute("INSERT INTO t (id, a) VALUES ('x', 'kept')", []).unwrap();

        // A later release adds a column and backfills it.
        let steps: &[&str] = &[
            "",
            "ALTER TABLE t ADD COLUMN b TEXT NOT NULL DEFAULT '';\n\
             UPDATE t SET b = 'backfilled' WHERE b = '';",
        ];
        apply_migrations(&conn, base, steps, 1).unwrap();

        let (a, b): (String, String) = conn
            .query_row("SELECT a, b FROM t WHERE id = 'x'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(a, "kept", "existing data survives");
        assert_eq!(b, "backfilled");
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn a_failing_migration_leaves_the_database_untouched() {
        let conn = Connection::open_in_memory().unwrap();
        let base = "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY);";
        apply_migrations(&conn, base, &[""], 0).unwrap();
        conn.execute("INSERT INTO t (id) VALUES ('x')", []).unwrap();

        let steps: &[&str] = &[
            "",
            "ALTER TABLE t ADD COLUMN b TEXT NOT NULL DEFAULT '';",
            "THIS IS NOT SQL;",
        ];
        assert!(apply_migrations(&conn, base, steps, 1).is_err());

        // Neither the partial change nor the version bump may survive.
        assert!(
            conn.query_row("SELECT b FROM t", [], |r| r.get::<_, String>(0)).is_err(),
            "the rolled-back column must be gone"
        );
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 1, "a half-applied migration must not claim success");
    }

    #[test]
    fn reopening_a_populated_database_is_a_no_op() {
        let path = std::env::temp_dir().join(format!("rc-store-{}.sqlite", ulid::Ulid::generate()));
        let s = Store::open(&path).unwrap();
        s.insert_task(&task("t1", "f", "k", "s", "queued"), "{}", "{}", "").unwrap();
        drop(s);

        let again = Store::open(&path).unwrap();
        assert_eq!(again.get_task("t1").unwrap().unwrap().status, "queued");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pending_ttl_reaps_abandoned_work_only() {
        let s = store();
        s.insert_task(&task("fresh", "f1", "k", "s", "queued"), "{}", "{}", "").unwrap();
        let mut old = task("old", "f2", "k", "s", "queued");
        old.created_at = now_ms() - 3_600_000;
        s.insert_task(&old, "{}", "{}", "").unwrap();
        let reaped = s.expire_pending(1800).unwrap();
        assert_eq!(reaped, vec!["old"]);
        assert_eq!(s.get_task("fresh").unwrap().unwrap().status, "queued");
    }

    #[test]
    fn a_result_stored_before_a_proto_field_existed_still_loads() {
        // `result()` swallows deserialisation errors, so a schema addition that
        // broke old rows would not fail loudly — every historical task would
        // just start rendering as if it had no result at all.
        // Every field the previous schema wrote is present; only `env_hints`,
        // which did not exist yet, is absent. A fixture missing more than that
        // would pass for the wrong reason.
        let row = TaskRow {
            result_json: r#"{"kind":"env_error","diagnostics":[],"error_count":0,"warning_count":0,
                "log_ref":"abc","stats":{"queue_ms":0,"sync_ms":348,"build_ms":8638,"upload_ms":0,
                "cache_hit_rate":0.0,"bytes_synced":14537},"summary":"环境错误（exit 101）",
                "exit_code":101,"truncated_diagnostics":0}"#
                .into(),
            ..Default::default()
        };
        let result = row.result().expect("an older row must still deserialise");
        assert_eq!(result.kind, "env_error");
        assert_eq!(result.exit_code, 101);
        assert_eq!(result.stats.unwrap().build_ms, 8638);
        assert!(result.env_hints.is_empty());
    }

    #[test]
    fn a_truncated_profile_still_fails_loudly_rather_than_defaulting() {
        // `try_dispatch` fails a task whose stored profile is unreadable,
        // because a profile silently emptied into defaults goes to a worker and
        // builds something other than what was asked for. Blanket
        // `#[serde(default)]` would have turned that failure into a success.
        let partial = r#"{"image":"registry/env@sha256:abc"}"#;
        assert!(
            serde_json::from_str::<rc_core::pb::ResolvedProfile>(partial).is_err(),
            "a profile missing its fields must not deserialise into defaults"
        );
    }
}
