import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import type { EChartsOption } from "echarts";
import { api, type Overview as OverviewData } from "../api";
import { Badge, Card, Empty, ErrorBox, Meter, Mono, Spinner, Stat, Table, Td, Th } from "../components/ui";
import { Chart, axisStyle, chartBase } from "../components/Chart";
import { ago, bytes, ms, percent, resultTone, shortId, statusTone } from "../lib/format";

export function Overview() {
  const q = useQuery({
    queryKey: ["overview"],
    queryFn: () => api.get<OverviewData>("/api/overview"),
    // Numbers refresh on a timer; state changes arrive over SSE (risk #19).
    refetchInterval: 10_000,
  });

  if (q.isLoading) return <Spinner />;
  if (q.isError) return <ErrorBox message={(q.error as Error).message} />;
  const d = q.data!;

  const throughput: EChartsOption = {
    ...chartBase,
    legend: { ...chartBase.legend, data: ["总数", "成功", "缓存命中"] },
    xAxis: {
      type: "category",
      data: d.histogram.map((h) => new Date(h.ts * 1000).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" })),
      ...axisStyle,
    },
    yAxis: { type: "value", ...axisStyle },
    series: [
      {
        name: "总数",
        type: "bar",
        data: d.histogram.map((h) => h.total),
        itemStyle: { color: "#243447", borderRadius: [2, 2, 0, 0] },
        barMaxWidth: 14,
      },
      {
        name: "成功",
        type: "bar",
        data: d.histogram.map((h) => h.success),
        itemStyle: { color: "#3fb950", borderRadius: [2, 2, 0, 0] },
        barMaxWidth: 14,
      },
      {
        name: "缓存命中",
        type: "line",
        smooth: true,
        symbol: "none",
        data: d.histogram.map((h) => h.cache_hit),
        lineStyle: { color: "#7c8cf8", width: 1.5 },
      },
    ],
  };

  const phases: EChartsOption = {
    ...chartBase,
    grid: { left: 52, right: 12, top: 24, bottom: 24 },
    legend: { ...chartBase.legend, data: ["p50", "p95"] },
    xAxis: { type: "value", ...axisStyle, axisLabel: { ...axisStyle.axisLabel, formatter: (v: number) => ms(v) } },
    yAxis: {
      type: "category",
      data: d.phase_percentiles.map((p) => phaseLabel(p.phase)),
      ...axisStyle,
    },
    series: [
      {
        name: "p50",
        type: "bar",
        data: d.phase_percentiles.map((p) => p.p50),
        itemStyle: { color: "#58a6ff", borderRadius: [0, 2, 2, 0] },
        barMaxWidth: 10,
      },
      {
        name: "p95",
        type: "bar",
        data: d.phase_percentiles.map((p) => p.p95),
        itemStyle: { color: "#d29922", borderRadius: [0, 2, 2, 0] },
        barMaxWidth: 10,
      },
    ],
  };

  const c = d.counters;

  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-3 lg:grid-cols-5">
        <Stat
          label="运行中"
          value={c.running}
          tone={c.running > 0 ? "info" : "muted"}
          hint={`队列 ${c.queued}`}
        />
        <Stat
          label="在线 Worker"
          value={d.workers_online}
          tone={d.workers_online === 0 ? "bad" : "ok"}
          hint={d.workers_online === 0 ? "所有任务都会排队" : `共 ${d.workers.length} 台连接`}
        />
        <Stat
          label="24h 成功率"
          value={percent(d.success_rate)}
          tone={d.success_rate >= 0.9 ? "ok" : d.success_rate >= 0.7 ? "warn" : "bad"}
          hint={`${c.success_window}/${c.finished_window} 个任务`}
        />
        <Stat
          label="缓存命中率"
          value={percent(d.cache_hit_rate)}
          tone={d.cache_hit_rate > 0.2 ? "ok" : "muted"}
          hint={`${c.cache_hits_window} 次未编译直接返回`}
        />
        <Stat
          label="24h 同步量"
          value={bytes(c.bytes_synced_window)}
          hint={`supersede ${c.superseded_window} · infra ${c.infra_errors_window} · 超时 ${c.timeouts_window}`}
        />
      </div>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-3">
        <Card title="任务吞吐（5 分钟桶）" className="xl:col-span-2">
          {d.histogram.length === 0 ? (
            <Empty>还没有任务数据。</Empty>
          ) : (
            <Chart option={throughput} height={220} />
          )}
        </Card>
        <Card title="各阶段耗时分位">
          {d.phase_percentiles.every((p) => p.p50 === 0 && p.p95 === 0) ? (
            <Empty>还没有足够样本。</Empty>
          ) : (
            <Chart option={phases} height={220} />
          )}
        </Card>
      </div>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-3">
        <Card title="Worker 资源池" className="xl:col-span-1" bodyClassName="p-0">
          {d.workers.length === 0 ? (
            <Empty>没有 worker 连接。用「设置 → Worker 注册」生成 enrollment token。</Empty>
          ) : (
            <Table>
              <tbody>
                {d.workers.map((w) => (
                  <tr key={w.id}>
                    <Td>
                      <Link to={`/workers/${w.id}`} className="hover:text-[var(--color-accent)]">
                        <Mono>{shortId(w.id, 14)}</Mono>
                      </Link>
                      <div className="mt-0.5">
                        <Badge tone={statusTone[w.status] ?? "muted"}>{w.status}</Badge>
                      </div>
                    </Td>
                    <Td>
                      <div className="flex items-center gap-2">
                        <Meter
                          value={w.stats.cpu_load}
                          tone={w.stats.cpu_load > 0.85 ? "warn" : "info"}
                        />
                        <span className="tnum text-[11px] text-[var(--color-ink-dim)]">
                          {percent(w.stats.cpu_load)}
                        </span>
                      </div>
                      <div className="tnum mt-1 text-[11px] text-[var(--color-ink-faint)]">
                        {w.stats.disk_free_gb}GB 空闲 · {w.stats.running_tasks}/{w.max_parallel} 任务
                      </div>
                    </Td>
                  </tr>
                ))}
              </tbody>
            </Table>
          )}
        </Card>

        <Card title="最近任务" className="xl:col-span-2" bodyClassName="p-0">
          {d.recent_tasks.length === 0 ? (
            <Empty>还没有任务。</Empty>
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>任务</Th>
                  <Th>类型</Th>
                  <Th>状态</Th>
                  <Th>耗时</Th>
                  <Th>时间</Th>
                </tr>
              </thead>
              <tbody>
                {d.recent_tasks.map((t) => (
                  <tr key={t.id} className="hover:bg-[var(--color-panel-2)]/50">
                    <Td>
                      <Link to={`/tasks/${t.id}`} className="hover:text-[var(--color-accent)]">
                        <Mono>{shortId(t.id, 12)}</Mono>
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
                    </Td>
                    <Td className="tnum text-[var(--color-ink-dim)]">{ms(t.build_ms)}</Td>
                    <Td className="text-[var(--color-ink-faint)]">{ago(t.created_at)}</Td>
                  </tr>
                ))}
              </tbody>
            </Table>
          )}
        </Card>
      </div>

      {d.alerts.length > 0 && (
        <Card title="未处理告警">
          <ul className="space-y-1.5">
            {d.alerts.map((a) => (
              <li key={a.id} className="flex items-center gap-2 text-[12px]">
                <Badge tone={a.level === "error" ? "bad" : "warn"}>{a.level}</Badge>
                <Mono className="text-[var(--color-ink-dim)]">{a.rule}</Mono>
                <span>{a.message}</span>
              </li>
            ))}
          </ul>
        </Card>
      )}
    </div>
  );
}

function phaseLabel(phase: string): string {
  return { queue: "排队", sync: "同步", build: "编译" }[phase] ?? phase;
}
