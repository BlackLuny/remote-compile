import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { EChartsOption } from "echarts";
import { api, type Overview, type Role, type StorageInfo } from "../api";
import { Button, Card, Empty, ErrorBox, Spinner, Stat } from "../components/ui";
import { Chart, axisStyle, chartBase } from "../components/Chart";
import { bytes, duration } from "../lib/format";

export function Storage({ role }: { role: Role }) {
  const qc = useQueryClient();

  const q = useQuery({
    queryKey: ["storage"],
    queryFn: () => api.get<StorageInfo>("/api/storage"),
    refetchInterval: 30_000,
  });

  const growth = useQuery({
    queryKey: ["series", "cas_bytes"],
    queryFn: () =>
      api.get<{ points: { ts: number; avg: number }[] }>(
        `/api/series?metric=cas_bytes&granularity=1min&since=${Math.floor(Date.now() / 1000) - 6 * 3600}`,
      ),
    refetchInterval: 60_000,
  });

  const reclaimed = useQuery({
    queryKey: ["overview", "gc"],
    queryFn: () => api.get<Overview>("/api/overview"),
    refetchInterval: 60_000,
  });

  const gc = useMutation({
    mutationFn: () => api.post<{ deleted: number; bytes: number }>("/api/storage/gc"),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["storage"] });
      qc.invalidateQueries({ queryKey: ["overview"] });
    },
  });

  if (q.isLoading) return <Spinner />;
  if (q.isError) return <ErrorBox message={(q.error as Error).message} />;
  const d = q.data!;

  const points = growth.data?.points ?? [];
  const trend: EChartsOption = {
    ...chartBase,
    legend: { show: false },
    xAxis: {
      type: "category",
      data: points.map((p) =>
        new Date(p.ts * 1000).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" }),
      ),
      ...axisStyle,
    },
    yAxis: {
      type: "value",
      ...axisStyle,
      axisLabel: { ...axisStyle.axisLabel, formatter: (v: number) => bytes(v) },
    },
    series: [
      {
        type: "line",
        smooth: true,
        symbol: "none",
        areaStyle: { color: "rgba(124,140,248,0.14)" },
        lineStyle: { color: "#7c8cf8", width: 1.5 },
        data: points.map((p) => p.avg),
      },
    ],
  };

  const counters = reclaimed.data?.metrics?.counters ?? {};

  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-3 lg:grid-cols-5">
        <Stat label="CAS 总量" value={bytes(d.tracked.bytes)} hint={`${d.tracked.blobs} 个 blob`} />
        <Stat
          label="磁盘实际占用"
          value={bytes(d.on_disk.bytes)}
          hint={`${d.on_disk.blobs} 个文件`}
          tone={d.on_disk.bytes > d.tracked.bytes * 1.2 ? "warn" : "muted"}
        />
        <Stat
          label="被任务 pin 住"
          value={d.tracked.pinned}
          tone={d.tracked.pinned > 0 ? "info" : "muted"}
          hint="租约期内，GC 不会回收（§4.7）"
        />
        <Stat
          label="可回收"
          value={d.collectable}
          tone={d.collectable > 0 ? "warn" : "ok"}
          hint={`TTL ${duration(d.policy.blob_gc_ttl_secs)}`}
        />
        <Stat
          label="累计回收"
          value={bytes(counters.gc_bytes_reclaimed_total ?? 0)}
          hint={`${counters.gc_blobs_deleted_total ?? 0} 个 blob`}
        />
      </div>

      <Card
        title="CAS 容量趋势（6 小时）"
        action={
          role === "admin" && (
            <Button onClick={() => gc.mutate()} disabled={gc.isPending}>
              {gc.isPending ? "回收中…" : "立即执行 GC"}
            </Button>
          )
        }
      >
        {points.length < 2 ? (
          <Empty>还没有足够的时序数据（rollup 每分钟落一次）。</Empty>
        ) : (
          <Chart option={trend} height={220} />
        )}
        {gc.data && (
          <div className="mt-3 text-[12px] text-[var(--color-ink-dim)]">
            上次手动 GC 回收了 {gc.data.deleted} 个 blob，释放 {bytes(gc.data.bytes)}。
          </div>
        )}
      </Card>

      <Card title="保留策略">
        <dl className="grid grid-cols-1 gap-3 text-[12px] sm:grid-cols-2 lg:grid-cols-4">
          <Policy
            label="CAS blob"
            value={duration(d.policy.blob_gc_ttl_secs)}
            note="无引用且冷却超时后删除；被任务 pin 住的永不回收"
          />
          <Policy
            label="构建日志"
            value={duration(d.policy.log_retention_secs)}
            note="zstd 压缩存储"
          />
          <Policy label="1min 时序" value="7 天" note="内建大盘的数据源" />
          <Policy label="1h 时序" value="90 天" note="长周期趋势" />
        </dl>
      </Card>
    </div>
  );
}

function Policy({ label, value, note }: { label: string; value: string; note: string }) {
  return (
    <div className="rounded border border-[var(--color-line-soft)] bg-[var(--color-surface)] px-3 py-2">
      <dt className="text-[11px] tracking-wide text-[var(--color-ink-dim)] uppercase">{label}</dt>
      <dd className="mt-0.5 text-[14px] font-semibold">{value}</dd>
      <dd className="mt-1 text-[11px] text-[var(--color-ink-faint)]">{note}</dd>
    </div>
  );
}
