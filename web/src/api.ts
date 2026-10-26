// Thin client over the admin REST API (§14.1). Cookies carry the session, so
// every request is credentialed; a 401 means "show the login screen", never
// "retry silently".

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

// How long any single call may take before the console gives up on it.
//
// A request that never settles is the worst failure mode here: the page spins
// with no way to tell "still working" from "never coming back". Bounded
// requests turn that into a visible error the user can act on.
const DEFAULT_TIMEOUT_MS = 20_000;

// Cleanup runs a disk reclaim on the worker and the server waits up to 300 s
// for the report, so its caller has to outlast that.
export const CLEANUP_TIMEOUT_MS = 320_000;

export interface RequestOpts {
  timeoutMs?: number;
}

async function request<T>(path: string, init?: RequestInit & RequestOpts): Promise<T> {
  const { timeoutMs = DEFAULT_TIMEOUT_MS, ...rest } = init ?? {};
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), timeoutMs);
  try {
    const res = await fetch(path, {
      credentials: "same-origin",
      headers: rest.body ? { "content-type": "application/json" } : undefined,
      ...rest,
      signal: abort.signal,
    });
    if (!res.ok) {
      let message = res.statusText;
      try {
        const body = await res.json();
        if (body?.error) message = body.error;
      } catch {
        // A non-JSON error body means the server is not the one we expect.
      }
      throw new ApiError(res.status, message);
    }
    if (res.status === 204) return undefined as T;
    return (await res.json()) as T;
  } catch (err) {
    // The abort surfaces as a plain AbortError; say what actually happened.
    if (abort.signal.aborted) {
      throw new ApiError(0, `请求超时（${Math.round(timeoutMs / 1000)} 秒）：${path}`);
    }
    throw err;
  } finally {
    clearTimeout(timer);
  }
}

export const api = {
  get: <T,>(path: string, opts?: RequestOpts) => request<T>(path, opts),
  post: <T,>(path: string, body?: unknown, opts?: RequestOpts) =>
    request<T>(path, { method: "POST", body: body ? JSON.stringify(body) : "{}", ...opts }),
  put: <T,>(path: string, body: unknown, opts?: RequestOpts) =>
    request<T>(path, { method: "PUT", body: JSON.stringify(body), ...opts }),
  del: <T,>(path: string, opts?: RequestOpts) => request<T>(path, { method: "DELETE", ...opts }),
};

// ---------------------------------------------------------------- types

export type Role = "admin" | "viewer";

export interface Me {
  username: string;
  role: Role;
}

export interface Task {
  id: string;
  task_type: string;
  project_id: string;
  worktree_id: string;
  agent_session: string;
  fingerprint: string;
  status: string;
  result_kind: string;
  command: string;
  image: string;
  log_ref: string;
  worker_id: string;
  // Display-only: the operator-given name for `worker_id`, or "" when the
  // machine has none (then the console falls back to the id).
  worker_name?: string;
  attempt: number;
  created_at: number;
  started_at: number;
  finished_at: number;
  error: string;
  superseded_by: string;
  queue_ms: number;
  sync_ms: number;
  build_ms: number;
  bytes_synced: number;
  cache_hit: number;
}

export interface Diagnostic {
  level: string;
  code: string;
  message: string;
  file: string;
  line: number;
  column: number;
  rendered: string;
}

export interface TaskResult {
  kind: string;
  diagnostics: Diagnostic[];
  error_count: number;
  warning_count: number;
  summary: string;
  exit_code: number;
  truncated_diagnostics: number;
  // Missing system dependencies read out of the build log; env_error only.
  env_hints?: string[];
  stats?: {
    queue_ms: number;
    sync_ms: number;
    build_ms: number;
    bytes_synced: number;
  };
}

export interface TimelinePhase {
  phase: string;
  at_ms: number;
  worker_id: string;
  detail: string;
}

export interface TaskDetail {
  task: Task;
  result: TaskResult | null;
  timeline: TimelinePhase[];
  attempts: { worker_id: string; worker_name?: string; at: number; error: string }[];
  placement: { worker_id: string; worker_name?: string; reason: string }[];
  profile: string | null;
  base_commit: string | null;
}

export interface WorkerStats {
  cpu_load: number;
  disk_free_gb: number;
  /** Filesystem size, 0 or absent from a worker that does not report one. */
  disk_total_gb?: number;
  running_tasks: number;
  cached_worktrees: string[];
  cached_projects: string[];
  cached_images: string[];
  sccache_hit_rate: number;
  gc_runs: number;
  gc_reclaimed_mb: number;
}

export interface Worker {
  id: string;
  name: string;
  arch: string;
  labels: string;
  capacity: string;
  status: string;
  version: string;
  max_parallel: number;
  enrolled_at: number;
  last_hb: number;
  connected: boolean;
  free_slots?: number;
  stats?: WorkerStats;
}

export interface WorkerCleanupResult {
  ok: boolean;
  request_id: string;
  reclaimed: number;
  skipped_active: number;
  skipped_fresh: number;
  disk_free_gb_before: number;
  disk_free_gb_after: number;
  reclaimed_worktrees: string[];
  message: string;
}

export interface WorkerCleanupReq {
  all_unused?: boolean;
  idle_days?: number;
}

export interface Alert {
  id: number;
  rule: string;
  level: string;
  message: string;
  at: number;
  resolved_at: number;
}

export interface Overview {
  counters: {
    running: number;
    queued: number;
    finished_window: number;
    success_window: number;
    cache_hits_window: number;
    superseded_window: number;
    infra_errors_window: number;
    timeouts_window: number;
    bytes_synced_window: number;
  };
  success_rate: number;
  cache_hit_rate: number;
  workers_online: number;
  workers: (Worker & { stats: WorkerStats })[];
  phase_percentiles: { phase: string; p50: number; p95: number }[];
  histogram: { ts: number; total: number; success: number; cache_hit: number }[];
  storage: { blobs: number; bytes: number; pinned: number };
  alerts: Alert[];
  recent_tasks: Task[];
  metrics: { counters: Record<string, number>; gauges: Record<string, number> };
}

export interface Image {
  id: string;
  image_ref: string;
  digest: string;
  dockerfile: string;
  pull_ref: string;
  status: string;
  arch: string;
  targets: string;
  description: string;
  created_by: string;
  approved_by: string;
  approved_at: number;
  last_success_at: number;
  success_count: number;
  total_count: number;
  consecutive_env_errors: number;
  built_at: number;
  created_at: number;
  build_log_ref: string;
  message: string;
}

export interface MirrorStatus {
  status: string;
  remote_ref: string;
  op: string;
  worker_id: string;
  message: string;
  at: number;
}

export interface ImageRow {
  image: Image;
  full_ref: string;
  remote_ref?: string;
  mirror?: MirrorStatus;
  health: { last_success_at: number; success_rate_7d: number; total_runs: number };
}

export interface Profile {
  id: string;
  project_id: string;
  path: string;
  adapter: string;
  image: string;
  config_toml: string;
  created_by: string;
  last_success_at: number;
  success_count: number;
  total_count: number;
  updated_at: number;
}

export interface Policy {
  task_cache_ttl_secs: number;
  pending_ttl_secs: number;
  blob_gc_ttl_secs: number;
  log_retention_secs: number;
  /** How long a finished task keeps its stored manifest (the big column). */
  manifest_retention_secs?: number;
  /** How long finished tasks keep their row, timeline and attempts. */
  task_retention_secs?: number;
  worker_offline_secs: number;
  max_infra_retries: number;
  require_image_approval: boolean;
  max_diagnostics: number;
  w_disk: number;
  w_cpu: number;
  w_cache_affinity: number;
  w_image_affinity: number;
  min_disk_free_gb: number;
  /** Hard floor as a share of each worker's own disk, e.g. 10 for "10%". */
  min_disk_free_percent: number;
  alert_webhook: string;
  default_image: string;
  /** External registry host, e.g. hub.covm.net */
  image_registry?: string;
  /** Repo name under the host, e.g. rc-env */
  image_registry_prefix?: string;
  /** Master switch for admin push/pull */
  image_registry_enabled?: boolean;
}

export interface StorageInfo {
  tracked: { blobs: number; bytes: number; pinned: number };
  on_disk: { blobs: number; bytes: number };
  collectable: number;
  policy: { blob_gc_ttl_secs: number; log_retention_secs: number };
}

export interface LogPage {
  lines: string[];
  offset: number;
  total_lines: number;
  truncated: boolean;
}

export interface AuditEntry {
  id: number;
  at: number;
  actor: string;
  action: string;
  target: string;
  detail: string;
}
