import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  api,
  type Worker,
  type WorkerCleanupReq,
  type WorkerCleanupResult,
  type WorkerCleanupSummary,
} from "../api";
import { Button, ErrorBox, Input, Modal, Mono } from "./ui";

const POLL_INTERVAL_MS = 1000;
const MAX_TRANSIENT_POLL_ERRORS = 5;

export function CleanupModal({
  worker,
  onClose,
}: {
  worker: Worker;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const [mode, setMode] = useState<"idle" | "all">("idle");
  const [idleDays, setIdleDays] = useState("7");
  const [activeRequestId, setActiveRequestId] = useState<string | null>(null);
  const [result, setResult] = useState<WorkerCleanupResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  // If a cleanup is already running on this worker (e.g. page was refreshed),
  // attach to it automatically so the operator sees live progress/outcome.
  useEffect(() => {
    let cancelled = false;
    api
      .get<WorkerCleanupSummary>(`/api/workers/${worker.id}/cleanup`)
      .then((summary) => {
        if (cancelled) return;
        if (summary.active && summary.active.status === "running") {
          setActiveRequestId(summary.active.request_id);
        }
      })
      .catch(() => {
        // Non-fatal: starting a new cleanup still works normally.
      });
    return () => {
      cancelled = true;
    };
  }, [worker.id]);

  // Poll the active cleanup job until it finishes or fails.
  useEffect(() => {
    if (!activeRequestId) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let pollErrors = 0;

    const poll = async () => {
      try {
        const job = await api.get<WorkerCleanupResult>(
          `/api/workers/${worker.id}/cleanup/${encodeURIComponent(activeRequestId)}`,
        );
        if (cancelled) return;
        pollErrors = 0;
        if (job.status === "done") {
          setActiveRequestId(null);
          setResult(job);
          setError(null);
          qc.invalidateQueries({ queryKey: ["workers"] });
          qc.invalidateQueries({ queryKey: ["worker", worker.id] });
          return;
        }
        if (job.status === "failed") {
          setActiveRequestId(null);
          setError(job.message || "清理失败");
          qc.invalidateQueries({ queryKey: ["workers"] });
          qc.invalidateQueries({ queryKey: ["worker", worker.id] });
          return;
        }
      } catch (err) {
        if (cancelled) return;
        const fatal = err instanceof ApiError && err.status >= 400 && err.status < 500;
        pollErrors += 1;
        if (fatal || pollErrors >= MAX_TRANSIENT_POLL_ERRORS) {
          setActiveRequestId(null);
          setError((err as Error).message || "查询清理状态失败");
          return;
        }
      }
      if (!cancelled) {
        timer = setTimeout(poll, POLL_INTERVAL_MS);
      }
    };

    timer = setTimeout(poll, POLL_INTERVAL_MS);
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [activeRequestId, qc, worker.id]);

  const start = useMutation({
    mutationFn: (body: WorkerCleanupReq) =>
      api.post<WorkerCleanupResult>(`/api/workers/${worker.id}/cleanup`, body),
    onMutate: () => {
      setError(null);
      setResult(null);
    },
    onSuccess: (job) => {
      if (job.status === "done") {
        setResult(job);
        qc.invalidateQueries({ queryKey: ["workers"] });
        qc.invalidateQueries({ queryKey: ["worker", worker.id] });
      } else if (job.status === "failed") {
        setError(job.message || "清理失败");
      } else {
        setActiveRequestId(job.request_id);
      }
    },
    onError: (err) => {
      setError((err as Error).message);
    },
  });

  const pending = start.isPending || activeRequestId !== null;
  const disk = worker.stats?.disk_free_gb;
  const caches = worker.stats?.cached_worktrees.length ?? 0;
  const running = worker.stats?.running_tasks ?? 0;

  return (
    <Modal title={`清理磁盘 · ${worker.name || worker.id}`} onClose={onClose}>
      <p className="mb-3 text-[12px] text-[var(--color-ink-dim)]">
        回收本机 worktree 的 target volume 与 workspace 目录。执行期间会临时停止接新任务；
        当前正在处理的 worktree 不会被清除。project registry / git mirror 不会动。
      </p>
      <div className="mb-3 grid grid-cols-3 gap-2 text-[12px]">
        <div className="rounded border border-[var(--color-line)] bg-[var(--color-surface)] px-2.5 py-2">
          <div className="text-[11px] text-[var(--color-ink-faint)]">磁盘余量</div>
          <div className="tnum font-medium">{disk != null ? `${disk} GB` : "–"}</div>
        </div>
        <div className="rounded border border-[var(--color-line)] bg-[var(--color-surface)] px-2.5 py-2">
          <div className="text-[11px] text-[var(--color-ink-faint)]">worktree 缓存</div>
          <div className="tnum font-medium">{caches}</div>
        </div>
        <div className="rounded border border-[var(--color-line)] bg-[var(--color-surface)] px-2.5 py-2">
          <div className="text-[11px] text-[var(--color-ink-faint)]">运行中任务</div>
          <div className="tnum font-medium">{running}</div>
        </div>
      </div>

      {!result && (
        <div className="space-y-2 text-[12px]">
          <label className="flex cursor-pointer items-start gap-2 rounded border border-[var(--color-line)] px-3 py-2 hover:bg-[var(--color-panel-2)]/40">
            <input
              type="radio"
              className="mt-0.5"
              checked={mode === "idle"}
              onChange={() => setMode("idle")}
              disabled={pending}
            />
            <span>
              <span className="font-medium">清理闲置超过 N 天</span>
              <span className="mt-1 flex items-center gap-2 text-[var(--color-ink-dim)]">
                <Input
                  type="number"
                  min={0}
                  className="w-16"
                  value={idleDays}
                  disabled={pending || mode !== "idle"}
                  onChange={(e) => setIdleDays(e.target.value)}
                />
                <span>天未使用的 worktree 缓存</span>
              </span>
              <span className="mt-1 flex flex-wrap gap-1">
                {[1, 3, 7, 14, 30].map((d) => (
                  <Button
                    key={d}
                    size="sm"
                    disabled={pending || mode !== "idle"}
                    onClick={() => {
                      setMode("idle");
                      setIdleDays(String(d));
                    }}
                  >
                    {d} 天
                  </Button>
                ))}
              </span>
            </span>
          </label>
          <label className="flex cursor-pointer items-start gap-2 rounded border border-[var(--color-line)] px-3 py-2 hover:bg-[var(--color-panel-2)]/40">
            <input
              type="radio"
              className="mt-0.5"
              checked={mode === "all"}
              onChange={() => setMode("all")}
              disabled={pending}
            />
            <span>
              <span className="font-medium">清理全部当前未使用</span>
              <div className="mt-0.5 text-[var(--color-ink-dim)]">
                删除所有没有运行中任务的 worktree 缓存（更激进，会丢掉近期但空闲的 target）
              </div>
            </span>
          </label>
        </div>
      )}

      {activeRequestId && !result && !error && (
        <div className="mt-3 rounded border border-[var(--color-line)] bg-[var(--color-surface)] px-3 py-2 text-[12px] text-[var(--color-ink-dim)]">
          清理正在控制面后台执行（<Mono>{activeRequestId.slice(0, 12)}</Mono>），可关闭弹窗稍后再来看结果。
        </div>
      )}

      {error && (
        <div className="mt-3">
          <ErrorBox message={error} />
        </div>
      )}

      {result && (
        <div className="mt-3 space-y-2 rounded border border-[var(--color-line)] bg-[var(--color-surface)] p-3 text-[12px]">
          <div className="font-medium text-[var(--color-ok)]">{result.message}</div>
          <div className="grid grid-cols-2 gap-2 tnum text-[var(--color-ink-dim)]">
            <div>回收 volume：{result.reclaimed}</div>
            <div>跳过活跃：{result.skipped_active}</div>
            <div>仍在新鲜期：{result.skipped_fresh}</div>
            <div>
              磁盘：{result.disk_free_gb_before} → {result.disk_free_gb_after} GB
            </div>
          </div>
          {result.reclaimed_worktrees.length > 0 && (
            <div>
              <div className="mb-1 text-[11px] text-[var(--color-ink-faint)]">已回收 worktree</div>
              <div className="flex max-h-28 flex-wrap gap-1 overflow-y-auto">
                {result.reclaimed_worktrees.map((w) => (
                  <Mono key={w} className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5">
                    {w.length > 16 ? `${w.slice(0, 16)}…` : w}
                  </Mono>
                ))}
              </div>
            </div>
          )}
        </div>
      )}

      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onClose}>{result ? "关闭" : pending ? "后台运行" : "取消"}</Button>
        {!result && (
          <Button
            variant="primary"
            disabled={pending || (mode === "idle" && Number.isNaN(Number(idleDays)))}
            onClick={() => {
              if (mode === "all") {
                start.mutate({ all_unused: true, idle_days: 0 });
              } else {
                const n = Math.max(0, Math.floor(Number(idleDays) || 0));
                start.mutate({ all_unused: false, idle_days: n });
              }
            }}
          >
            {pending ? "清理中…" : "开始清理"}
          </Button>
        )}
      </div>
    </Modal>
  );
}
