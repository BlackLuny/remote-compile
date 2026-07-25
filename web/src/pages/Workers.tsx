import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api, type Role, type Worker } from "../api";
import {
  Badge,
  Button,
  Card,
  Empty,
  ErrorBox,
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
    </div>
  );
}
