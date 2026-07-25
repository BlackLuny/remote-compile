import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api, type Task } from "../api";
import { Badge, Button, Card, Empty, ErrorBox, Input, Mono, Select, Spinner, Table, Td, Th } from "../components/ui";
import { ago, bytes, ms, resultTone, shortId, statusTone } from "../lib/format";

const PAGE = 50;

export function Tasks() {
  const [status, setStatus] = useState("");
  const [kind, setKind] = useState("");
  const [type, setType] = useState("");
  const [project, setProject] = useState("");
  const [session, setSession] = useState("");
  const [page, setPage] = useState(0);

  const params = new URLSearchParams();
  if (status) params.set("status", status);
  if (kind) params.set("result_kind", kind);
  if (type) params.set("task_type", type);
  if (project) params.set("project_id", project);
  if (session) params.set("agent_session", session);
  params.set("limit", String(PAGE));
  params.set("offset", String(page * PAGE));

  const q = useQuery({
    queryKey: ["tasks", params.toString()],
    queryFn: () => api.get<{ tasks: Task[]; total: number }>(`/api/tasks?${params}`),
    refetchInterval: 10_000,
  });

  const reset = <T,>(setter: (v: T) => void) => (v: T) => {
    setPage(0);
    setter(v);
  };

  const total = q.data?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / PAGE));

  return (
    <Card
      title="任务"
      action={
        <div className="flex flex-wrap items-center gap-2">
          <Input
            placeholder="project id"
            value={project}
            onChange={(e) => reset(setProject)(e.target.value)}
            className="w-40"
          />
          <Input
            placeholder="agent session"
            value={session}
            onChange={(e) => reset(setSession)(e.target.value)}
            className="w-40"
          />
          <Select
            value={type}
            onChange={reset(setType)}
            options={[
              { value: "", label: "全部类型" },
              { value: "check", label: "check" },
              { value: "build", label: "build" },
              { value: "test", label: "test" },
              { value: "clippy", label: "clippy" },
            ]}
          />
          <Select
            value={status}
            onChange={reset(setStatus)}
            options={[
              { value: "", label: "全部状态" },
              { value: "queued", label: "排队中" },
              { value: "running", label: "运行中" },
              { value: "done", label: "已完成" },
              { value: "failed", label: "失败" },
              { value: "superseded", label: "被取代" },
              { value: "canceled", label: "已取消" },
            ]}
          />
          <Select
            value={kind}
            onChange={reset(setKind)}
            options={[
              { value: "", label: "全部结果" },
              { value: "success", label: "success" },
              { value: "compile_error", label: "compile_error" },
              { value: "env_error", label: "env_error" },
              { value: "infra_error", label: "infra_error" },
              { value: "timeout", label: "timeout" },
            ]}
          />
        </div>
      }
      bodyClassName="p-0"
    >
      {q.isLoading ? (
        <Spinner />
      ) : q.isError ? (
        <div className="p-4">
          <ErrorBox message={(q.error as Error).message} />
        </div>
      ) : q.data!.tasks.length === 0 ? (
        <Empty>没有匹配的任务。</Empty>
      ) : (
        <>
          <Table>
            <thead>
              <tr>
                <Th>任务</Th>
                <Th>类型</Th>
                <Th>结果</Th>
                <Th>Worktree</Th>
                <Th>Worker</Th>
                <Th className="text-right">排队</Th>
                <Th className="text-right">同步</Th>
                <Th className="text-right">编译</Th>
                <Th className="text-right">传输</Th>
                <Th>创建</Th>
              </tr>
            </thead>
            <tbody>
              {q.data!.tasks.map((t) => (
                <tr key={t.id} className="hover:bg-[var(--color-panel-2)]/50">
                  <Td>
                    <Link to={`/tasks/${t.id}`} className="hover:text-[var(--color-accent)]">
                      <Mono>{shortId(t.id, 14)}</Mono>
                    </Link>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{t.task_type}</Td>
                  <Td>
                    {t.result_kind ? (
                      <Badge tone={resultTone[t.result_kind] ?? "muted"}>{t.result_kind}</Badge>
                    ) : (
                      <Badge tone={statusTone[t.status] ?? "muted"}>{t.status}</Badge>
                    )}
                    {t.cache_hit === 1 && (
                      <Badge tone="accent" className="ml-1">
                        cache
                      </Badge>
                    )}
                    {t.attempt > 1 && (
                      <Badge tone="warn" className="ml-1" >
                        {t.attempt} 次尝试
                      </Badge>
                    )}
                  </Td>
                  <Td>
                    <Mono className="text-[var(--color-ink-dim)]">{shortId(t.worktree_id, 12)}</Mono>
                  </Td>
                  <Td>
                    <Mono className="text-[var(--color-ink-dim)]">
                      {t.worker_id ? shortId(t.worker_id, 12) : "–"}
                    </Mono>
                  </Td>
                  <Td className="tnum text-right text-[var(--color-ink-dim)]">{ms(t.queue_ms)}</Td>
                  <Td className="tnum text-right text-[var(--color-ink-dim)]">{ms(t.sync_ms)}</Td>
                  <Td className="tnum text-right text-[var(--color-ink-dim)]">{ms(t.build_ms)}</Td>
                  <Td className="tnum text-right text-[var(--color-ink-dim)]">
                    {t.bytes_synced ? bytes(t.bytes_synced) : "–"}
                  </Td>
                  <Td className="text-[var(--color-ink-faint)]">{ago(t.created_at)}</Td>
                </tr>
              ))}
            </tbody>
          </Table>

          <div className="flex items-center justify-between px-4 py-2.5 text-[11.5px] text-[var(--color-ink-dim)]">
            <span className="tnum">
              第 {page + 1} / {pages} 页 · 共 {total} 个
            </span>
            <div className="flex gap-2">
              <Button onClick={() => setPage((p) => Math.max(0, p - 1))} disabled={page === 0}>
                上一页
              </Button>
              <Button onClick={() => setPage((p) => p + 1)} disabled={page + 1 >= pages}>
                下一页
              </Button>
            </div>
          </div>
        </>
      )}
    </Card>
  );
}
