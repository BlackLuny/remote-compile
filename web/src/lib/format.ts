// Formatting shared by every page. Operators compare these values across
// rows, so they must be consistent and compact.

export function bytes(n: number): string {
  if (!n) return "0";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${unit === 0 ? value : value.toFixed(1)}${units[unit]}`;
}

export function ms(n: number): string {
  if (!n) return "–";
  if (n < 1000) return `${Math.round(n)}ms`;
  if (n < 60_000) return `${(n / 1000).toFixed(1)}s`;
  const m = Math.floor(n / 60_000);
  const s = Math.round((n % 60_000) / 1000);
  return `${m}m${s.toString().padStart(2, "0")}s`;
}

/** Relative time from a millisecond epoch. `0` means "never". */
export function ago(msEpoch: number): string {
  if (!msEpoch) return "从未";
  const delta = Math.max(0, Date.now() - msEpoch);
  if (delta < 5_000) return "刚刚";
  if (delta < 60_000) return `${Math.floor(delta / 1000)} 秒前`;
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} 分钟前`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} 小时前`;
  return `${Math.floor(delta / 86_400_000)} 天前`;
}

/** Same, for second-granularity timestamps. */
export function agoSecs(secEpoch: number): string {
  return ago(secEpoch ? secEpoch * 1000 : 0);
}

export function clock(msEpoch: number): string {
  if (!msEpoch) return "–";
  return new Date(msEpoch).toLocaleString("zh-CN", { hour12: false });
}

export function percent(x: number): string {
  return `${Math.round(x * 100)}%`;
}

export function duration(secs: number): string {
  if (secs % 86400 === 0) return `${secs / 86400} 天`;
  if (secs % 3600 === 0) return `${secs / 3600} 小时`;
  if (secs % 60 === 0) return `${secs / 60} 分钟`;
  return `${secs} 秒`;
}

/** Shorten an id for display while keeping it recognisable. */
export function shortId(id: string, keep = 10): string {
  if (id.length <= keep + 3) return id;
  return `${id.slice(0, keep)}…`;
}

export function shortDigest(ref: string): string {
  const at = ref.indexOf("@");
  if (at < 0) return ref;
  return `${ref.slice(0, at)}@${ref.slice(at + 1, at + 15)}…`;
}

/** Colour semantics, defined once so status never means two things. */
export const resultTone: Record<string, "ok" | "bad" | "warn" | "info" | "muted"> = {
  success: "ok",
  compile_error: "bad",
  env_error: "warn",
  infra_error: "warn",
  timeout: "warn",
};

export const statusTone: Record<string, "ok" | "bad" | "warn" | "info" | "muted"> = {
  done: "ok",
  running: "info",
  uploading: "info",
  queued: "muted",
  pending: "muted",
  syncing: "muted",
  failed: "bad",
  canceled: "muted",
  superseded: "muted",
  online: "ok",
  draining: "warn",
  offline: "muted",
  healthy: "ok",
  building: "info",
  pending_approval: "warn",
  failing: "bad",
  rejected: "muted",
};
