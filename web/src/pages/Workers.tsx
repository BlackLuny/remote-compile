import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import {
  api,
  type Role,
  type Worker,
  type WorkerCleanupReq,
  type WorkerCleanupResult,
} from "../api";
import {
  Badge,
  Button,
  Card,
  Empty,
  ErrorBox,
  Input,
  Meter,
  Modal,
  Mono,
  Spinner,
  Table,
  Td,
  Th,
} from "../components/ui";
import { agoSecs, percent, statusTone } from "../lib/format";

export function Workers({ role }: { role: Role }) {
  const qc = useQueryClient();
  const [token, setToken] = useState<string | null>(null);
  const [cleanupFor, setCleanupFor] = useState<Worker | null>(null);
  const [cleanupResult, setCleanupResult] = useState<WorkerCleanupResult | null>(null);

  const q = useQuery({
    queryKey: ["workers"],
    queryFn: () => api.get<{ workers: Worker[] }>("/api/workers"),
    refetchInterval: 5000,
  });

  const act = useMutation({
    mutationFn: ({ id, action }: { id: string; action: string }) =>
      action === "delete"
        ? api.del(`/api/workers/${id}`)
        : api.post(`/api/workers/${id}/${action}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["workers"] }),
  });

  const cleanup = useMutation({
    mutationFn: ({ id, body }: { id: string; body: WorkerCleanupReq }) =>
      api.post<WorkerCleanupResult>(`/api/workers/${id}/cleanup`, body),
    onSuccess: (data) => {
      setCleanupResult(data);
      qc.invalidateQueries({ queryKey: ["workers"] });
    },
  });

  const enroll = useMutation({
    mutationFn: () => api.post<{ token: string; expires_in: number }>("/api/enrollment-tokens", {}),
    onSuccess: (data) => setToken(data.token),
  });

  return (
    <div className="space-y-4">
      <Card
        title="Worker"
        action={
          role === "admin" && (
            <Button variant="primary" onClick={() => enroll.mutate()} disabled={enroll.isPending}>
              生成 enrollment token
            </Button>
          )
        }
        bodyClassName="p-0"
      >
        {q.isLoading ? (
          <Spinner />
        ) : q.isError ? (
          <div className="p-4">
            <ErrorBox message={(q.error as Error).message} />
          </div>
        ) : q.data!.workers.length === 0 ? (
          <Empty>
            还没有 worker。在编译机上执行 <Mono>rc-worker enroll --server … --token …</Mono>
          </Empty>
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>Worker</Th>
                <Th>状态</Th>
                <Th>CPU</Th>
                <Th>磁盘</Th>
                <Th>任务</Th>
                <Th>缓存</Th>
                <Th>版本</Th>
                <Th>心跳</Th>
                {role === "admin" && <Th />}
              </tr>
            </thead>
            <tbody>
              {q.data!.workers.map((w) => (
                <tr key={w.id} className="hover:bg-[var(--color-panel-2)]/50">
                  <Td>
                    <Link to={`/workers/${w.id}`} className="hover:text-[var(--color-accent)]">
                      <Mono>{w.id}</Mono>
                    </Link>
                    <div className="text-[11px] text-[var(--color-ink-faint)]">{w.arch}</div>
                  </Td>
                  <Td>
                    <Badge tone={statusTone[w.connected ? w.status : "offline"] ?? "muted"}>
                      {w.connected ? w.status : "offline"}
                    </Badge>
                  </Td>
                  <Td>
                    {w.stats ? (
                      <div className="flex items-center gap-2">
                        <Meter
                          value={w.stats.cpu_load}
                          tone={w.stats.cpu_load > 0.85 ? "warn" : "info"}
                        />
                        <span className="tnum text-[11px]">{percent(w.stats.cpu_load)}</span>
                      </div>
                    ) : (
                      <span className="text-[var(--color-ink-faint)]">–</span>
                    )}
                  </Td>
                  <Td className="tnum">{w.stats ? `${w.stats.disk_free_gb} GB` : "–"}</Td>
                  <Td className="tnum">
                    {w.stats ? `${w.stats.running_tasks}/${w.max_parallel}` : `–/${w.max_parallel}`}
                  </Td>
                  <Td className="tnum text-[var(--color-ink-dim)]">
                    {w.stats
                      ? `${w.stats.cached_worktrees.length} worktree · ${w.stats.cached_projects.length} project`
                      : "–"}
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{w.version}</Td>
                  <Td className="text-[var(--color-ink-faint)]">{agoSecs(w.last_hb)}</Td>
                  {role === "admin" && (
                    <Td>
                      <div className="flex justify-end gap-1.5">
                        {w.status === "draining" ? (
                          <Button onClick={() => act.mutate({ id: w.id, action: "resume" })}>
                            恢复
                          </Button>
                        ) : (
                          <Button
                            onClick={() => act.mutate({ id: w.id, action: "drain" })}
                            title="不接新任务，跑完存量"
                          >
                            Drain
                          </Button>
                        )}
                        <Button
                          disabled={!w.connected}
                          title={
                            w.connected
                              ? "手动清理 worktree 缓存以释放磁盘"
                              : "worker 未连接"
                          }
                          onClick={() => {
                            setCleanupResult(null);
                            cleanup.reset();
                            setCleanupFor(w);
                          }}
                        >
                          清理磁盘
                        </Button>
                        <Button
                          variant="danger"
                          onClick={() => {
                            if (confirm(`从资源池移除 ${w.id}？运行中的任务会重新排队。`)) {
                              act.mutate({ id: w.id, action: "delete" });
                            }
                          }}
                        >
                          移除
                        </Button>
                      </div>
                    </Td>
                  )}
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Card>

      {token && (
        <Modal title="Enrollment token" onClose={() => setToken(null)}>
          <p className="mb-3 text-[12px] text-[var(--color-ink-dim)]">
            单次使用、1 小时内有效。这是唯一一次显示，关闭后无法再取回。
          </p>
          <div className="rounded border border-[var(--color-line)] bg-[var(--color-surface)] p-3 font-[var(--font-mono)] text-[11.5px] break-all">
            {token}
          </div>
          <p className="mt-3 text-[12px] text-[var(--color-ink-dim)]">在编译机上执行：</p>
          <div className="mt-1 rounded border border-[var(--color-line)] bg-[var(--color-surface)] p-3 font-[var(--font-mono)] text-[11.5px] break-all">
            rc-worker enroll --server {location.protocol}//{location.hostname}:7701 --token {token}
          </div>
          <div className="mt-4 flex justify-end gap-2">
            <Button onClick={() => navigator.clipboard?.writeText(token)}>复制 token</Button>
            <Button variant="primary" onClick={() => setToken(null)}>
              我已保存
            </Button>
          </div>
        </Modal>
      )}

      {cleanupFor && (
        <CleanupModal
          worker={cleanupFor}
          pending={cleanup.isPending}
          error={cleanup.isError ? (cleanup.error as Error).message : null}
          result={cleanupResult}
          onClose={() => {
            if (cleanup.isPending) return;
            setCleanupFor(null);
            setCleanupResult(null);
            cleanup.reset();
          }}
          onRun={(body) => cleanup.mutate({ id: cleanupFor.id, body })}
        />
      )}
    </div>
  );
}

function CleanupModal({
  worker,
  pending,
  error,
  result,
  onClose,
  onRun,
}: {
  worker: Worker;
  pending: boolean;
  error: string | null;
  result: WorkerCleanupResult | null;
  onClose: () => void;
  onRun: (body: WorkerCleanupReq) => void;
}) {
  const [mode, setMode] = useState<"idle" | "all">("idle");
  const [idleDays, setIdleDays] = useState("7");

  const disk = worker.stats?.disk_free_gb;
  const caches = worker.stats?.cached_worktrees.length ?? 0;
  const running = worker.stats?.running_tasks ?? 0;

  return (
    <Modal title={`清理磁盘 · ${worker.id}`} onClose={onClose}>
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
        <Button onClick={onClose} disabled={pending}>
          {result ? "关闭" : "取消"}
        </Button>
        {!result && (
          <Button
            variant="primary"
            disabled={pending || (mode === "idle" && Number.isNaN(Number(idleDays)))}
            onClick={() => {
              if (mode === "all") {
                onRun({ all_unused: true, idle_days: 0 });
              } else {
                const n = Math.max(0, Math.floor(Number(idleDays) || 0));
                onRun({ all_unused: false, idle_days: n });
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
