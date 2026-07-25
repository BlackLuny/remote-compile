import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type Alert, type Me } from "../api";
import { useEventStream, type RcEvent } from "../lib/sse";
import { Badge, Button } from "./ui";
import { useCallback, useState } from "react";

const NAV = [
  { to: "/", label: "大盘", end: true },
  { to: "/tasks", label: "任务" },
  { to: "/workers", label: "Worker" },
  { to: "/images", label: "镜像" },
  { to: "/profiles", label: "构建档案" },
  { to: "/storage", label: "存储" },
  { to: "/settings", label: "设置" },
];

export function Layout({ me }: { me: Me }) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [alertsOpen, setAlertsOpen] = useState(false);

  const alerts = useQuery({
    queryKey: ["alerts"],
    queryFn: () => api.get<{ alerts: Alert[] }>("/api/alerts"),
    refetchInterval: 30_000,
  });

  // State changes arrive over SSE; the query cache is invalidated rather than
  // patched, so the UI never drifts from what the server actually holds.
  const onEvent = useCallback(
    (event: RcEvent) => {
      switch (event.type) {
        case "task_updated":
          qc.invalidateQueries({ queryKey: ["tasks"] });
          qc.invalidateQueries({ queryKey: ["task", event.task_id] });
          qc.invalidateQueries({ queryKey: ["overview"] });
          break;
        case "worker_updated":
          qc.invalidateQueries({ queryKey: ["workers"] });
          qc.invalidateQueries({ queryKey: ["overview"] });
          break;
        case "image_updated":
          qc.invalidateQueries({ queryKey: ["images"] });
          break;
        case "alert":
          qc.invalidateQueries({ queryKey: ["alerts"] });
          break;
        case "queue_depth":
          qc.invalidateQueries({ queryKey: ["overview"] });
          break;
      }
    },
    [qc],
  );
  const live = useEventStream(onEvent);

  const openAlerts = alerts.data?.alerts ?? [];

  const logout = async () => {
    await api.post("/api/logout");
    qc.clear();
    navigate("/login", { replace: true });
  };

  return (
    <div className="flex h-full">
      <aside className="flex w-52 shrink-0 flex-col border-r border-[var(--color-line)] bg-[var(--color-panel)]">
        <div className="flex items-center gap-2 px-4 py-4">
          <div className="h-6 w-6 rounded bg-[var(--color-accent)]/20 text-center text-[13px] leading-6 font-bold text-[var(--color-accent)]">
            rc
          </div>
          <div>
            <div className="text-[13px] leading-none font-semibold">remote-compile</div>
            <div className="mt-1 text-[10px] text-[var(--color-ink-faint)]">控制台</div>
          </div>
        </div>

        <nav className="flex-1 px-2">
          {NAV.map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              end={item.end}
              className={({ isActive }) =>
                clsx(
                  "mb-0.5 block rounded px-3 py-1.5 text-[12.5px] transition-colors",
                  isActive
                    ? "bg-[var(--color-panel-2)] font-medium text-[var(--color-ink)]"
                    : "text-[var(--color-ink-dim)] hover:bg-[var(--color-panel-2)]/60 hover:text-[var(--color-ink)]",
                )
              }
            >
              {item.label}
            </NavLink>
          ))}
        </nav>

        <div className="border-t border-[var(--color-line-soft)] px-4 py-3">
          <div className="flex items-center justify-between">
            <div>
              <div className="text-[12px]">{me.username}</div>
              <div className="text-[10px] text-[var(--color-ink-faint)]">
                {me.role === "admin" ? "管理员" : "只读"}
              </div>
            </div>
            <Button variant="ghost" onClick={logout}>
              退出
            </Button>
          </div>
        </div>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-11 shrink-0 items-center justify-end gap-3 border-b border-[var(--color-line)] bg-[var(--color-panel)]/60 px-5">
          <button
            onClick={() => setAlertsOpen((v) => !v)}
            className="relative flex items-center gap-1.5 rounded px-2 py-1 text-[12px] text-[var(--color-ink-dim)] hover:bg-[var(--color-panel-2)] hover:text-[var(--color-ink)]"
          >
            告警
            {openAlerts.length > 0 && (
              <Badge tone={openAlerts.some((a) => a.level === "error") ? "bad" : "warn"}>
                {openAlerts.length}
              </Badge>
            )}
          </button>
          <div
            className="flex items-center gap-1.5 text-[11px] text-[var(--color-ink-faint)]"
            title={live ? "实时推送已连接" : "实时推送断开，正在退避重连"}
          >
            <span
              className={clsx(
                "inline-block h-1.5 w-1.5 rounded-full",
                live ? "live-dot bg-[var(--color-ok)]" : "bg-[var(--color-ink-faint)]",
              )}
            />
            {live ? "实时" : "重连中"}
          </div>
        </header>

        {alertsOpen && (
          <div className="fade-in border-b border-[var(--color-line)] bg-[var(--color-panel)] px-5 py-3">
            {openAlerts.length === 0 ? (
              <div className="text-[12px] text-[var(--color-ink-faint)]">没有未处理告警。</div>
            ) : (
              <ul className="space-y-1.5">
                {openAlerts.map((a) => (
                  <li key={a.id} className="flex items-center gap-2 text-[12px]">
                    <Badge tone={a.level === "error" ? "bad" : "warn"}>{a.level}</Badge>
                    <span className="text-[var(--color-ink-dim)]">{a.rule}</span>
                    <span>{a.message}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}

        <main className="min-h-0 flex-1 overflow-y-auto p-5">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
