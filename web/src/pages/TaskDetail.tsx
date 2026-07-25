import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import type { EChartsOption } from "echarts";
import { api, type Role, type TaskDetail as Detail } from "../api";
import { Badge, Button, Card, Empty, ErrorBox, Mono, Spinner, Table, Td, Th } from "../components/ui";
import { Chart, axisStyle, chartBase } from "../components/Chart";
import { LogViewer } from "../components/LogViewer";
import { bytes, clock, ms, resultTone, shortDigest, statusTone } from "../lib/format";

export function TaskDetail({ role }: { role: Role }) {
  const { id = "" } = useParams();
  const qc = useQueryClient();

  const q = useQuery({
    queryKey: ["task", id],
    queryFn: () => api.get<Detail>(`/api/tasks/${id}`),
    refetchInterval: (query) => {
      const status = query.state.data?.task.status;
      return status && !["done", "failed", "canceled", "superseded"].includes(status)
        ? 3000
        : false;
    },
  });

  if (q.isLoading) return <Spinner />;
  if (q.isError) return <ErrorBox message={(q.error as Error).message} />;
  const { task, result, timeline, attempts, placement, profile, base_commit } = q.data!;

  const cancel = async () => {
    await api.post(`/api/tasks/${id}/cancel`);
    qc.invalidateQueries({ queryKey: ["task", id] });
  };

  const active = !["done", "failed", "canceled", "superseded"].includes(task.status);

  return (
    <div className="space-y-4">
      <Card
        title={
          <span className="flex items-center gap-2">
            任务 <Mono className="normal-case">{task.id}</Mono>
          </span>
        }
        action={
          <div className="flex items-center gap-2">
            {result?.kind ? (
              <Badge tone={resultTone[result.kind] ?? "muted"}>{result.kind}</Badge>
            ) : (
              <Badge tone={statusTone[task.status] ?? "muted"}>{task.status}</Badge>
            )}
            {task.cache_hit === 1 && <Badge tone="accent">缓存命中</Badge>}
            {role === "admin" && active && (
              <Button variant="danger" onClick={cancel}>
                取消任务
              </Button>
            )}
          </div>
        }
      >
        <div className="grid grid-cols-2 gap-x-6 gap-y-2 text-[12px] lg:grid-cols-4">
          <Field label="类型" value={task.task_type} />
          <Field label="状态" value={task.status} />
          <Field label="Worker" value={task.worker_id || "–"} mono />
          <Field label="尝试次数" value={String(task.attempt)} />
          <Field label="Project" value={task.project_id} mono />
          <Field label="Worktree" value={task.worktree_id} mono />
          <Field label="Agent session" value={task.agent_session} mono />
          <Field label="Base commit" value={base_commit ? base_commit.slice(0, 12) : "（无基线）"} mono />
          <Field label="镜像" value={task.image ? shortDigest(task.image) : "–"} mono />
          <Field label="指纹" value={task.fingerprint.slice(0, 16)} mono />
          <Field label="创建于" value={clock(task.created_at)} />
          <Field label="结束于" value={clock(task.finished_at)} />
        </div>
        <div className="mt-3 border-t border-[var(--color-line-soft)] pt-3">
          <div className="text-[11px] tracking-wide text-[var(--color-ink-dim)] uppercase">命令</div>
          <Mono className="mt-1 block break-all text-[var(--color-ink)]">{task.command}</Mono>
        </div>
        {task.superseded_by && (
          <div className="mt-3 text-[12px] text-[var(--color-ink-dim)]">
            被更新的代码取代 →{" "}
            <Link to={`/tasks/${task.superseded_by}`} className="text-[var(--color-accent)]">
              <Mono>{task.superseded_by}</Mono>
            </Link>
          </div>
        )}
      </Card>

      {placement.length > 0 && (
        <Card title="为什么还在排队">
          <Table>
            <thead>
              <tr>
                <Th>Worker</Th>
                <Th>被排除的原因</Th>
              </tr>
            </thead>
            <tbody>
              {placement.map((p) => (
                <tr key={p.worker_id}>
                  <Td>
                    <Mono>{p.worker_id}</Mono>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{explainReject(p.reason)}</Td>
                </tr>
              ))}
            </tbody>
          </Table>
        </Card>
      )}

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <Card title="阶段时间线">
          {timeline.length === 0 ? (
            <Empty>还没有阶段记录。</Empty>
          ) : (
            <>
              <Chart option={waterfall(task.created_at, timeline)} height={Math.max(140, timeline.length * 26)} />
              <div className="tnum mt-2 flex gap-4 text-[11px] text-[var(--color-ink-dim)]">
                <span>排队 {ms(task.queue_ms)}</span>
                <span>同步 {ms(task.sync_ms)}</span>
                <span>编译 {ms(task.build_ms)}</span>
                <span>传输 {bytes(task.bytes_synced)}</span>
              </div>
            </>
          )}
        </Card>

        <Card title="诊断">
          {!result || result.diagnostics.length === 0 ? (
            <Empty>{result?.summary || "没有结构化诊断。"}</Empty>
          ) : (
            <div className="space-y-2">
              <div className="text-[12px] text-[var(--color-ink-dim)]">{result.summary}</div>
              {result.diagnostics.map((d, i) => (
                <div
                  key={i}
                  className="rounded border border-[var(--color-line-soft)] bg-[var(--color-surface)] p-2"
                >
                  <div className="flex items-center gap-2">
                    <Badge tone={d.level === "error" ? "bad" : "warn"}>{d.level}</Badge>
                    {d.code && <Mono className="text-[var(--color-ink-dim)]">{d.code}</Mono>}
                    {d.file && (
                      <Mono className="text-[var(--color-ink-faint)]">
                        {d.file}:{d.line}:{d.column}
                      </Mono>
                    )}
                  </div>
                  <div className="mt-1 text-[12px]">{d.message}</div>
                  {d.rendered && (
                    <pre className="mt-1.5 overflow-x-auto font-[var(--font-mono)] text-[11px] leading-[16px] whitespace-pre text-[var(--color-ink-dim)]">
                      {d.rendered}
                    </pre>
                  )}
                </div>
              ))}
              {result.truncated_diagnostics > 0 && (
                <div className="text-[11.5px] text-[var(--color-ink-faint)]">
                  另有 {result.truncated_diagnostics} 条诊断未存储，完整内容见下方日志。
                </div>
              )}
            </div>
          )}
        </Card>
      </div>

      {attempts.length > 0 && (
        <Card title="重试记录">
          <Table>
            <thead>
              <tr>
                <Th>Worker</Th>
                <Th>时间</Th>
                <Th>错误</Th>
              </tr>
            </thead>
            <tbody>
              {attempts.map((a, i) => (
                <tr key={i}>
                  <Td>
                    <Mono>{a.worker_id}</Mono>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{clock(a.at)}</Td>
                  <Td className="text-[var(--color-ink-dim)]">{a.error}</Td>
                </tr>
              ))}
            </tbody>
          </Table>
        </Card>
      )}

      <Card title="构建日志" bodyClassName="p-0">
        <LogViewer taskId={task.id} />
      </Card>

      {profile && profile !== "null" && (
        <Card title="解析后的构建档案">
          <pre className="overflow-x-auto font-[var(--font-mono)] text-[11px] leading-[16px] text-[var(--color-ink-dim)]">
            {prettyJson(profile)}
          </pre>
        </Card>
      )}
    </div>
  );
}

function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div>
      <div className="text-[11px] tracking-wide text-[var(--color-ink-dim)] uppercase">{label}</div>
      <div className="mt-0.5 break-all">
        {mono ? <Mono>{value}</Mono> : <span>{value}</span>}
      </div>
    </div>
  );
}

/**
 * Phase waterfall (§15.3): the point is to see at a glance whether a slow task
 * was slow in the queue, in sync, or in the compiler.
 */
function waterfall(createdAt: number, timeline: { phase: string; at_ms: number }[]): EChartsOption {
  const rows = timeline.map((p, i) => {
    const start = p.at_ms - createdAt;
    const end = (timeline[i + 1]?.at_ms ?? p.at_ms) - createdAt;
    return { phase: p.phase, start, span: Math.max(end - start, 1) };
  });
  return {
    ...chartBase,
    legend: { show: false },
    grid: { left: 96, right: 16, top: 8, bottom: 24 },
    xAxis: {
      type: "value",
      ...axisStyle,
      axisLabel: { ...axisStyle.axisLabel, formatter: (v: number) => ms(v) },
    },
    yAxis: { type: "category", data: rows.map((r) => r.phase), ...axisStyle, inverse: true },
    series: [
      {
        type: "bar",
        stack: "t",
        itemStyle: { color: "transparent" },
        data: rows.map((r) => r.start),
        silent: true,
      },
      {
        type: "bar",
        stack: "t",
        barMaxWidth: 12,
        itemStyle: { color: "#58a6ff", borderRadius: 2 },
        data: rows.map((r) => r.span),
        tooltip: { valueFormatter: (v) => ms(Number(v)) },
      },
    ],
  };
}

function explainReject(reason: string): string {
  return (
    {
      NotOnline: "worker 不在线或正在 drain",
      ArchMismatch: "架构不匹配",
      NoFreeSlot: "并行槽位已满",
      InsufficientDisk: "磁盘余量低于预估需求",
      AlreadyTried: "该 worker 已失败过，重试不会回到同一台（§6.2）",
      WorktreeBusy: "同一 worktree 已有任务在跑，串行执行（§6.2）",
    }[reason] ?? reason
  );
}

function prettyJson(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}
