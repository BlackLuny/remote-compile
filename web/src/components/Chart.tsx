// A thin ECharts wrapper. ECharts is used rather than a React-native chart
// library because ops dashboards need dense time series, brushing and large
// point counts without re-rendering the tree (§14.1).

import { useEffect, useRef } from "react";
import * as echarts from "echarts/core";
import { BarChart, LineChart } from "echarts/charts";
import {
  GridComponent,
  LegendComponent,
  TooltipComponent,
  DataZoomComponent,
} from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";
import type { EChartsOption } from "echarts";

echarts.use([
  LineChart,
  BarChart,
  GridComponent,
  TooltipComponent,
  LegendComponent,
  DataZoomComponent,
  CanvasRenderer,
]);

/** Shared axis/tooltip styling so every chart reads as one system. */
export const chartBase: EChartsOption = {
  grid: { left: 44, right: 12, top: 24, bottom: 24 },
  tooltip: {
    trigger: "axis",
    backgroundColor: "#151e28",
    borderColor: "#1e2a36",
    textStyle: { color: "#e6edf3", fontSize: 11 },
    axisPointer: { lineStyle: { color: "#2b3947" } },
  },
  legend: {
    textStyle: { color: "#93a4b3", fontSize: 11 },
    icon: "roundRect",
    itemWidth: 8,
    itemHeight: 8,
    top: 0,
    right: 0,
  },
};

export const axisStyle = {
  axisLine: { lineStyle: { color: "#1e2a36" } },
  axisLabel: { color: "#5f7182", fontSize: 10 },
  splitLine: { lineStyle: { color: "#141e27" } },
};

export function Chart({
  option,
  height = 200,
  className,
}: {
  option: EChartsOption;
  height?: number;
  className?: string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const instance = useRef<echarts.ECharts | null>(null);

  useEffect(() => {
    if (!host.current) return;
    instance.current = echarts.init(host.current, undefined, { renderer: "canvas" });
    // Charts live in resizable panels; without this they stay at their
    // initial width forever.
    const observer = new ResizeObserver(() => instance.current?.resize());
    observer.observe(host.current);
    return () => {
      observer.disconnect();
      instance.current?.dispose();
      instance.current = null;
    };
  }, []);

  useEffect(() => {
    // `notMerge: false` keeps zoom state across data refreshes.
    instance.current?.setOption(option, { notMerge: false, lazyUpdate: true });
  }, [option]);

  return <div ref={host} style={{ height }} className={className} />;
}
