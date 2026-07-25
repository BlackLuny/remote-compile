import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import type { EChartsOption } from "echarts";
import { api, type Role, type Task, type Worker, type WorkerStats } from "../api";
import { Badge, Card, Empty, ErrorBox, Mono, Spinner, Stat, Table, Td, Th } from "../components/ui";
import { Chart, axisStyle, chartBase } from "../components/Chart";
import { agoSecs, ms, percent, resultTone, shortId, statusTone } from "../lib/format";

interface Sample {
  t: number;
  cpu: number;
  disk: number;
}

export function WorkerDetail({ role }: { role: Role }) {
  const { id = "" } = useParams();
  const [history, setHistory] = useState<Sample[]>([]);

  const q = useQuery({
    queryKey: ["worker", id],
    queryFn: () =>
      api.get<{ worker: Worker; live: (Worker & { stats: WorkerStats }) | null; running_tasks: Task[] }>(
        `/api/workers/${id}`,
      ),
    refetchInterval: 3000,
  });

  // Heartbeat stats live in the control plane's memory and are never persisted
  // at that frequency (§15.1), so the curve is built client-side from the
  // samples this session has seen.
  const live = q.data?.live;
  useEffect(() => {
    if (!live?.stats) return;
    setHistory((prev) =>
      [...prev, { t: Date.now(), cpu: live.stats.cpu_load, disk: live.stats.disk_free_gb }].slice(-120),
    );
  }, [live]);

  if (q.isLoading) return <Spinner />;
  if (q.isError) return <ErrorBox message={(q.error as Error).message} />;
  const { worker, running_tasks } = q.data!;

  const trend: EChartsOption = {
    ...chartBase,
    legend: { ...chartBase.legend, data: ["CPU 负载", "磁盘余量 GB"] },
    xAxis: {
      type: "category",
      data: history.map((s) => new Date(s.t).toLocaleTimeString("zh-CN", { hour12: false })),
      ...axisStyle,
    },
    yAxis: [
      { type: "value", max: 1, ...axisStyle, axisLabel: { ...axisStyle.axisLabel, formatter: (v: number) => percent(v) } },
      { type: "value", ...axisStyle, splitLine: { show: false } },
    ],
    series: [
      {
        name: "CPU 负载",
        type: "line",
        smooth: true,
        symbol: "none",
        areaStyle: { color: "rgba(88,166,255,0.12)" },
        lineStyle: { color: "#58a6ff", width: 1.5 },
        data: history.map((s) => s.cpu),
      },
      {
        name: "磁盘余量 GB",
        type: "line",
        yAxisIndex: 1,
        smooth: true,
        symbol: "none",
        lineStyle: { color: "#3fb950", width: 1.5 },
        data: history.map((s) => s.disk),
      },
    ],
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <Link to="/workers" className="text-[12px] text-[var(--color-ink-dim)] hover:text-[var(--color-ink)]">
            ← 返回 Worker 列表
          </Link>
          <h1 className="mt-1 text-[16px] font-semibold">
            <Mono>{worker.id}</Mono>
          </h1>
        </div>
        <Badge tone={statusTone[live ? worker.status : "offline"] ?? "muted"}>
          {live ? worker.status : "offline"}
        </Badge>
      </div>

      <div className="grid grid-cols-2 gap-3 lg:grid-cols-5">
        <Stat label="CPU 负载" value={live ? percent(live.stats.cpu_load) : "–"} tone="info" />
        <Stat
          label="磁盘余量"
          value={live ? `${live.stats.disk_free_gb} GB` : "–"}
          tone={live && live.stats.disk_free_gb < 50 ? "warn" : "ok"}
        />
        <Stat
          label="运行中任务"
          value={`${live?.stats.running_tasks ?? 0}/${worker.max_parallel}`}
        />
        <Stat label="架构" value={worker.arch || "–"} />
        <Stat label="最近心跳" value={agoSecs(worker.last_hb)} hint={`版本 ${worker.version}`} />
      </div>

      <Card title="资源曲线（本会话采样）">
        {history.length < 2 ? (
          <Empty>正在采集心跳样本…</Empty>
        ) : (
          <Chart option={trend} height={220} />
        )}
      </Card>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <Card title="运行中的任务" bodyClassName="p-0">
          {running_tasks.length === 0 ? (
            <Empty>当前空闲。</Empty>
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>任务</Th>
                  <Th>类型</Th>
                  <Th>状态</Th>
                  <Th>已耗时</Th>
                </tr>
              </thead>
              <tbody>
                {running_tasks.map((t) => (
                  <tr key={t.id}>
                    <Td>
                      <Link to={`/tasks/${t.id}`} className="hover:text-[var(--color-accent)]">
                        <Mono>{shortId(t.id, 14)}</Mono>
                      </Link>
                    </Td>
                    <Td className="text-[var(--color-ink-dim)]">{t.task_type}</Td>
                    <Td>
                      <Badge tone={resultTone[t.result_kind] ?? statusTone[t.status] ?? "muted"}>
                        {t.result_kind || t.status}
                      </Badge>
                    </Td>
                    <Td className="tnum text-[var(--color-ink-dim)]">
                      {t.started_at ? ms(Date.now() - t.started_at) : "–"}
                    </Td>
                  </tr>
                ))}
              </tbody>
            </Table>
          )}
        </Card>

        <Card title="本地缓存">
          {!live ? (
            <Empty>worker 未连接，无法读取缓存清单。</Empty>
          ) : (
            <div className="space-y-3 text-[12px]">
              <CacheList
                label="Worktree target volume"
                hint="调度亲和的主要依据（§6.1）"
                items={live.stats.cached_worktrees}
              />
              <CacheList label="Project git mirror" items={live.stats.cached_projects} />
              <CacheList label="Docker volume" items={live.stats.cached_images} />
              {role === "admin" && (
                <p className="text-[11px] text-[var(--color-ink-faint)]">
                  空闲超过策略阈值的 worktree 缓存由 worker 自行回收（§9）。
                </p>
              )}
            </div>
          )}
        </Card>
      </div>
    </div>
  );
}

function CacheList({ label, hint, items }: { label: string; hint?: string; items: string[] }) {
  return (
    <div>
      <div className="flex items-baseline gap-2">
        <span className="text-[11px] tracking-wide text-[var(--color-ink-dim)] uppercase">{label}</span>
        <span className="tnum text-[11px] text-[var(--color-ink-faint)]">{items.length}</span>
      </div>
      {hint && <div className="text-[11px] text-[var(--color-ink-faint)]">{hint}</div>}
      <div className="mt-1 flex flex-wrap gap-1">
        {items.length === 0 ? (
          <span className="text-[11px] text-[var(--color-ink-faint)]">（空）</span>
        ) : (
          items.slice(0, 24).map((x) => (
            <Mono key={x} className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5">
              {shortId(x, 16)}
            </Mono>
          ))
        )}
        {items.length > 24 && (
          <span className="text-[11px] text-[var(--color-ink-faint)]">
            +{items.length - 24}
          </span>
        )}
      </div>
    </div>
  );
}
