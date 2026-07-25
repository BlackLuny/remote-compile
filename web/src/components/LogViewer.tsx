// Build-log viewer (§14.3): virtualised, greppable, ANSI-aware, with a tail
// mode. Full logs run to tens of thousands of lines, so the server pages them
// and this renders only what is on screen.

import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, type LogPage } from "../api";
import { Button, Empty, ErrorBox, Input, Spinner } from "./ui";

const ROW_HEIGHT = 18;
const OVERSCAN = 20;
const PAGE_SIZE = 1000;

export function LogViewer({ taskId }: { taskId: string }) {
  const [grep, setGrep] = useState("");
  const [query, setQuery] = useState("");
  const [offset, setOffset] = useState(0);
  const [follow, setFollow] = useState(false);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewport, setViewport] = useState(600);
  const host = useRef<HTMLDivElement>(null);

  const params = new URLSearchParams({
    limit: String(PAGE_SIZE),
    offset: String(offset),
  });
  if (query) params.set("grep", query);
  if (follow) params.set("tail", "true");

  const q = useQuery({
    queryKey: ["log", taskId, params.toString()],
    queryFn: () => api.get<LogPage>(`/api/tasks/${taskId}/log?${params}`),
    // Only poll while following a live build.
    refetchInterval: follow ? 3000 : false,
  });

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const observer = new ResizeObserver(() => setViewport(el.clientHeight));
    observer.observe(el);
    setViewport(el.clientHeight);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (follow && host.current) host.current.scrollTop = host.current.scrollHeight;
  }, [follow, q.data]);

  const lines = q.data?.lines ?? [];
  const first = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const visible = Math.ceil(viewport / ROW_HEIGHT) + OVERSCAN * 2;
  const slice = useMemo(
    () => lines.slice(first, first + visible),
    [lines, first, visible],
  );

  const applyGrep = (e: React.FormEvent) => {
    e.preventDefault();
    setOffset(0);
    setQuery(grep);
  };

  return (
    <div className="flex h-[70vh] flex-col">
      <div className="flex flex-wrap items-center gap-2 border-b border-[var(--color-line-soft)] px-3 py-2">
        <form onSubmit={applyGrep} className="flex items-center gap-1.5">
          <Input
            placeholder="grep（如 error）"
            value={grep}
            onChange={(e) => setGrep(e.target.value)}
            className="w-48"
          />
          <Button type="submit">筛选</Button>
          {query && (
            <Button
              variant="ghost"
              onClick={() => {
                setGrep("");
                setQuery("");
                setOffset(0);
              }}
            >
              清除
            </Button>
          )}
        </form>
        <Button variant={follow ? "primary" : "subtle"} onClick={() => setFollow((v) => !v)}>
          {follow ? "跟随中" : "跟随末尾"}
        </Button>
        <div className="tnum ml-auto text-[11px] text-[var(--color-ink-faint)]">
          {q.data
            ? `${q.data.offset}–${q.data.offset + lines.length} / ${q.data.total_lines} 行`
            : ""}
        </div>
        <div className="flex gap-1.5">
          <Button
            onClick={() => setOffset((o) => Math.max(0, o - PAGE_SIZE))}
            disabled={offset === 0 || follow}
          >
            上一页
          </Button>
          <Button
            onClick={() => setOffset((o) => o + PAGE_SIZE)}
            disabled={!q.data?.truncated || follow}
          >
            下一页
          </Button>
        </div>
      </div>

      {q.isLoading ? (
        <Spinner />
      ) : q.isError ? (
        <div className="p-3">
          <ErrorBox message={(q.error as Error).message} />
        </div>
      ) : lines.length === 0 ? (
        <Empty>
          {query ? `没有匹配 “${query}” 的行。` : "该任务没有日志（可能是缓存命中）。"}
        </Empty>
      ) : (
        <div
          ref={host}
          onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
          className="flex-1 overflow-auto bg-[var(--color-surface)] px-3 py-2"
        >
          <div style={{ height: lines.length * ROW_HEIGHT, position: "relative" }}>
            <div style={{ transform: `translateY(${first * ROW_HEIGHT}px)` }}>
              {slice.map((line, i) => (
                <div
                  key={first + i}
                  style={{ height: ROW_HEIGHT }}
                  className="flex gap-3 font-[var(--font-mono)] text-[11.5px] leading-[18px] whitespace-pre"
                >
                  <span className="tnum w-12 shrink-0 text-right text-[var(--color-ink-faint)] select-none">
                    {(q.data?.offset ?? 0) + first + i + 1}
                  </span>
                  <span className="min-w-0">{renderAnsi(line)}</span>
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

const SGR_COLOR: Record<number, string> = {
  30: "#5f7182",
  31: "#f85149",
  32: "#3fb950",
  33: "#d29922",
  34: "#58a6ff",
  35: "#bc8cff",
  36: "#39c5cf",
  37: "#e6edf3",
  90: "#5f7182",
  91: "#ff7b72",
  92: "#56d364",
  93: "#e3b341",
  94: "#79c0ff",
  95: "#d2a8ff",
  96: "#56d4dd",
  97: "#f0f6fc",
};

/**
 * Render SGR escape sequences as styled spans. Logs are stored raw so the
 * console can show them the way the compiler wrote them; the MCP surface gets
 * them stripped instead (§11).
 */
function renderAnsi(line: string): React.ReactNode {
  if (!line.includes("\u001b[")) return line;

  const parts: React.ReactNode[] = [];
  const pattern = /\u001b\[([0-9;]*)m/g;
  let cursor = 0;
  let style: React.CSSProperties = {};
  let match: RegExpExecArray | null;
  let key = 0;

  while ((match = pattern.exec(line)) !== null) {
    if (match.index > cursor) {
      parts.push(
        <span key={key++} style={style}>
          {line.slice(cursor, match.index)}
        </span>,
      );
    }
    style = applySgr(style, match[1]);
    cursor = match.index + match[0].length;
  }
  if (cursor < line.length) {
    parts.push(
      <span key={key++} style={style}>
        {line.slice(cursor)}
      </span>,
    );
  }
  return parts;
}

function applySgr(current: React.CSSProperties, codes: string): React.CSSProperties {
  const next = { ...current };
  for (const raw of codes.split(";")) {
    const code = Number(raw || "0");
    if (code === 0) return {};
    if (code === 1) next.fontWeight = 600;
    else if (code === 2) next.opacity = 0.7;
    else if (code === 3) next.fontStyle = "italic";
    else if (code === 4) next.textDecoration = "underline";
    else if (code === 22) next.fontWeight = undefined;
    else if (code === 39) next.color = undefined;
    else if (SGR_COLOR[code]) next.color = SGR_COLOR[code];
  }
  return next;
}
