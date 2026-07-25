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

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    credentials: "same-origin",
    headers: init?.body ? { "content-type": "application/json" } : undefined,
    ...init,
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
}

export const api = {
  get: <T,>(path: string) => request<T>(path),
  post: <T,>(path: string, body?: unknown) =>
    request<T>(path, { method: "POST", body: body ? JSON.stringify(body) : "{}" }),
  put: <T,>(path: string, body: unknown) =>
    request<T>(path, { method: "PUT", body: JSON.stringify(body) }),
  del: <T,>(path: string) => request<T>(path, { method: "DELETE" }),
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
  attempts: { worker_id: string; at: number; error: string }[];
  placement: { worker_id: string; reason: string }[];
  profile: string | null;
  base_commit: string | null;
}

export interface WorkerStats {
  cpu_load: number;
  disk_free_gb: number;
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

export interface ImageRow {
  image: Image;
  full_ref: string;
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
  worker_offline_secs: number;
  max_infra_retries: number;
  require_image_approval: boolean;
  max_diagnostics: number;
  w_disk: number;
  w_cpu: number;
  w_cache_affinity: number;
  w_image_affinity: number;
  min_disk_free_gb: number;
  alert_webhook: string;
  default_image: string;
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
