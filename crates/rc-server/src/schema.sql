-- rc-server storage schema (§18).
-- Reference counts are derived from task_blob_refs; there is deliberately no
-- ref_count column, because two sources of truth for the same fact always
-- drift apart.

CREATE TABLE IF NOT EXISTS projects (
  id          TEXT PRIMARY KEY,
  repo_url    TEXT NOT NULL DEFAULT '',
  root_path   TEXT NOT NULL DEFAULT '',
  created_at  INTEGER NOT NULL DEFAULT 0,
  last_seen   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS worktrees (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL DEFAULT '',
  label       TEXT NOT NULL DEFAULT '',
  last_seen   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_worktrees_project ON worktrees(project_id);

CREATE TABLE IF NOT EXISTS profiles (
  id               TEXT PRIMARY KEY,
  project_id       TEXT NOT NULL,
  path             TEXT NOT NULL DEFAULT '',
  adapter          TEXT NOT NULL DEFAULT '',
  image            TEXT NOT NULL DEFAULT '',
  config_toml      TEXT NOT NULL DEFAULT '',
  created_by       TEXT NOT NULL DEFAULT '',
  last_success_at  INTEGER NOT NULL DEFAULT 0,
  success_count    INTEGER NOT NULL DEFAULT 0,
  total_count      INTEGER NOT NULL DEFAULT 0,
  updated_at       INTEGER NOT NULL DEFAULT 0,
  UNIQUE(project_id, path)
);

CREATE TABLE IF NOT EXISTS images (
  id                     TEXT PRIMARY KEY,
  image_ref              TEXT NOT NULL DEFAULT '',
  digest                 TEXT NOT NULL DEFAULT '',
  dockerfile             TEXT NOT NULL DEFAULT '',
  pull_ref               TEXT NOT NULL DEFAULT '',
  status                 TEXT NOT NULL DEFAULT 'pending_approval',
  arch                   TEXT NOT NULL DEFAULT '',
  targets                TEXT NOT NULL DEFAULT '',
  description            TEXT NOT NULL DEFAULT '',
  created_by             TEXT NOT NULL DEFAULT '',
  approved_by            TEXT NOT NULL DEFAULT '',
  approved_at            INTEGER NOT NULL DEFAULT 0,
  last_success_at        INTEGER NOT NULL DEFAULT 0,
  success_count          INTEGER NOT NULL DEFAULT 0,
  total_count            INTEGER NOT NULL DEFAULT 0,
  consecutive_env_errors INTEGER NOT NULL DEFAULT 0,
  last_env_error_project TEXT NOT NULL DEFAULT '',
  built_at               INTEGER NOT NULL DEFAULT 0,
  created_at             INTEGER NOT NULL DEFAULT 0,
  build_log_ref          TEXT NOT NULL DEFAULT '',
  message                TEXT NOT NULL DEFAULT ''
);
-- Hosts a project's builds may reach beyond the fleet default (§7.1). The
-- repository asks; an administrator decides. Scoped to one project on purpose:
-- an allowlist entry is a channel data can be encoded out through (§16), so one
-- project's dependency source is not another project's build script's business.
CREATE TABLE IF NOT EXISTS egress (
  project_id   TEXT NOT NULL,
  host         TEXT NOT NULL,
  status       TEXT NOT NULL DEFAULT 'pending_approval',
  reason       TEXT NOT NULL DEFAULT '',
  requested_by TEXT NOT NULL DEFAULT '',
  approved_by  TEXT NOT NULL DEFAULT '',
  approved_at  INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, host)
);
CREATE INDEX IF NOT EXISTS idx_egress_status ON egress(status);

-- Fleet-learned `pre_commands`. The rest of a profile decides which image and
-- which command a build uses; this is arbitrary shell running inside another
-- project's sandbox, so the fleet may not pass it on unasked. A repository
-- running its own `pre_commands` needs no approval — approval is for teaching
-- them to agents that never asked. Keyed by content digest, so editing the
-- script asks again.
CREATE TABLE IF NOT EXISTS pre_commands (
  project_id   TEXT NOT NULL,
  path         TEXT NOT NULL DEFAULT '',
  digest       TEXT NOT NULL,
  commands     TEXT NOT NULL DEFAULT '',
  status       TEXT NOT NULL DEFAULT 'pending_approval',
  requested_by TEXT NOT NULL DEFAULT '',
  approved_by  TEXT NOT NULL DEFAULT '',
  approved_at  INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, path, digest)
);
CREATE INDEX IF NOT EXISTS idx_pre_commands_status ON pre_commands(status);

CREATE INDEX IF NOT EXISTS idx_images_status ON images(status);
CREATE INDEX IF NOT EXISTS idx_images_digest ON images(digest);

CREATE TABLE IF NOT EXISTS workers (
  id            TEXT PRIMARY KEY,
  arch          TEXT NOT NULL DEFAULT '',
  labels        TEXT NOT NULL DEFAULT '{}',
  capacity      TEXT NOT NULL DEFAULT '{}',
  status        TEXT NOT NULL DEFAULT 'offline',
  version       TEXT NOT NULL DEFAULT '',
  max_parallel  INTEGER NOT NULL DEFAULT 1,
  enrolled_at   INTEGER NOT NULL DEFAULT 0,
  last_hb       INTEGER NOT NULL DEFAULT 0,
  token_hash    TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS tasks (
  id             TEXT PRIMARY KEY,
  task_type      TEXT NOT NULL,
  project_id     TEXT NOT NULL DEFAULT '',
  worktree_id    TEXT NOT NULL DEFAULT '',
  agent_session  TEXT NOT NULL DEFAULT '',
  fingerprint    TEXT NOT NULL DEFAULT '',
  supersede_key  TEXT NOT NULL DEFAULT '',
  status         TEXT NOT NULL DEFAULT 'pending',
  result_kind    TEXT NOT NULL DEFAULT '',
  command        TEXT NOT NULL DEFAULT '',
  image          TEXT NOT NULL DEFAULT '',
  log_ref        TEXT NOT NULL DEFAULT '',
  worker_id      TEXT NOT NULL DEFAULT '',
  attempt        INTEGER NOT NULL DEFAULT 0,
  created_at     INTEGER NOT NULL DEFAULT 0,
  started_at     INTEGER NOT NULL DEFAULT 0,
  finished_at    INTEGER NOT NULL DEFAULT 0,
  error          TEXT NOT NULL DEFAULT '',
  superseded_by  TEXT NOT NULL DEFAULT '',
  result_json    TEXT NOT NULL DEFAULT '',
  queue_ms       INTEGER NOT NULL DEFAULT 0,
  sync_ms        INTEGER NOT NULL DEFAULT 0,
  build_ms       INTEGER NOT NULL DEFAULT 0,
  bytes_synced   INTEGER NOT NULL DEFAULT 0,
  cache_hit      INTEGER NOT NULL DEFAULT 0,
  egress_key     TEXT NOT NULL DEFAULT '',
  units_seen_total INTEGER NOT NULL DEFAULT 0
);
-- `egress_key` is the egress grant this task's fingerprint was computed from
-- (§7.1), so the build cannot run with a grant its cache key does not describe.
-- It lives outside the CREATE above because SQLite reconstructs that statement
-- verbatim when a column is dropped, and a comment inside the body makes the
-- reconstruction unparseable.
CREATE INDEX IF NOT EXISTS idx_tasks_fingerprint ON tasks(fingerprint);
CREATE INDEX IF NOT EXISTS idx_tasks_supersede   ON tasks(supersede_key, status);
CREATE INDEX IF NOT EXISTS idx_tasks_status      ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_tasks_created     ON tasks(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_worker      ON tasks(worker_id, status);
CREATE INDEX IF NOT EXISTS idx_tasks_project     ON tasks(project_id, created_at DESC);

-- Manifests and profiles are large; keeping them out of `tasks` keeps list
-- queries cheap.
CREATE TABLE IF NOT EXISTS task_inputs (
  task_id       TEXT PRIMARY KEY,
  manifest_json TEXT NOT NULL DEFAULT '',
  profile_json  TEXT NOT NULL DEFAULT '',
  base_commit   TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS task_events (
  task_id   TEXT NOT NULL,
  phase     TEXT NOT NULL,
  at_ms     INTEGER NOT NULL,
  worker_id TEXT NOT NULL DEFAULT '',
  detail    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_task_events ON task_events(task_id, at_ms);

CREATE TABLE IF NOT EXISTS task_attempts (
  task_id   TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  at        INTEGER NOT NULL,
  error     TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (task_id, worker_id)
);

-- Which sessions are waiting on a task. A task with foreign subscribers must
-- not be superseded (§5.2, risk #23).
CREATE TABLE IF NOT EXISTS task_subs (
  task_id       TEXT NOT NULL,
  agent_session TEXT NOT NULL,
  at            INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (task_id, agent_session)
);

CREATE TABLE IF NOT EXISTS cas_blobs (
  hash      TEXT PRIMARY KEY,
  size      INTEGER NOT NULL DEFAULT 0,
  last_used INTEGER NOT NULL DEFAULT 0,
  pinned    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_blobs_lru ON cas_blobs(pinned, last_used);

CREATE TABLE IF NOT EXISTS task_blob_refs (
  task_id TEXT NOT NULL,
  hash    TEXT NOT NULL,
  PRIMARY KEY (task_id, hash)
);
CREATE INDEX IF NOT EXISTS idx_blob_refs_hash ON task_blob_refs(hash);

-- Commits the fleet can already materialize, and the bundles that got them
-- there (§4.1).
CREATE TABLE IF NOT EXISTS project_commits (
  project_id TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  at         INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, commit_sha)
);

CREATE TABLE IF NOT EXISTS project_bundles (
  project_id TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  blob_hash  TEXT NOT NULL,
  at         INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, commit_sha)
);

CREATE TABLE IF NOT EXISTS admins (
  username      TEXT PRIMARY KEY,
  password_hash TEXT NOT NULL,
  role          TEXT NOT NULL DEFAULT 'viewer',
  created_at    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sessions (
  token      TEXT PRIMARY KEY,
  username   TEXT NOT NULL,
  role       TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT 0,
  expires_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS agent_tokens (
  token_hash TEXT PRIMARY KEY,
  label      TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT 0,
  last_used  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS enrollment_tokens (
  token      TEXT PRIMARY KEY,
  created_by TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT 0,
  expires_at INTEGER NOT NULL DEFAULT 0,
  used_at    INTEGER NOT NULL DEFAULT 0,
  used_by    TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS audit_log (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  at     INTEGER NOT NULL,
  actor  TEXT NOT NULL DEFAULT '',
  action TEXT NOT NULL DEFAULT '',
  target TEXT NOT NULL DEFAULT '',
  detail TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS alerts (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  rule        TEXT NOT NULL,
  level       TEXT NOT NULL DEFAULT 'warn',
  message     TEXT NOT NULL DEFAULT '',
  at          INTEGER NOT NULL DEFAULT 0,
  resolved_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_alerts_open ON alerts(rule, resolved_at);

CREATE TABLE IF NOT EXISTS settings (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL DEFAULT ''
);

-- Built-in time series (§15.1): 1min buckets kept 7 days, 1h buckets 90 days.
CREATE TABLE IF NOT EXISTS metrics_rollup (
  metric      TEXT NOT NULL,
  granularity TEXT NOT NULL,
  bucket_ts   INTEGER NOT NULL,
  sum         REAL NOT NULL DEFAULT 0,
  count       INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (metric, granularity, bucket_ts)
);
CREATE INDEX IF NOT EXISTS idx_rollup_read ON metrics_rollup(metric, granularity, bucket_ts);
