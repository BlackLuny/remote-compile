// Live updates (§14.1, risk #19).
//
// SSE rather than WebSocket: the traffic is one-way and the browser reconnects
// on its own. Two rules keep it from becoming a load problem:
//   * only *state changes* arrive here — chart data is polled;
//   * reconnects back off exponentially, so a control-plane restart does not
//     turn every open console into a retry storm.

import { useEffect, useRef, useState } from "react";

export interface RcEvent {
  type:
    | "task_updated"
    | "worker_updated"
    | "image_updated"
    | "alert"
    | "queue_depth";
  [key: string]: unknown;
}

const MAX_BACKOFF_MS = 30_000;

export function useEventStream(onEvent: (event: RcEvent) => void, enabled = true) {
  const [connected, setConnected] = useState(false);
  const handler = useRef(onEvent);
  handler.current = onEvent;

  useEffect(() => {
    if (!enabled) return;
    let source: EventSource | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let backoff = 1000;
    let closed = false;

    const connect = () => {
      if (closed) return;
      source = new EventSource("/api/events");

      source.onopen = () => {
        setConnected(true);
        backoff = 1000;
      };

      source.addEventListener("update", (e) => {
        try {
          handler.current(JSON.parse((e as MessageEvent).data) as RcEvent);
        } catch {
          // A malformed frame is not worth tearing the stream down for.
        }
      });

      source.onerror = () => {
        setConnected(false);
        source?.close();
        // EventSource retries on its own, but without a ceiling; drive it
        // ourselves so a long outage does not hammer the server.
        timer = setTimeout(connect, backoff);
        backoff = Math.min(backoff * 2, MAX_BACKOFF_MS);
      };
    };

    connect();
    return () => {
      closed = true;
      if (timer) clearTimeout(timer);
      source?.close();
      setConnected(false);
    };
  }, [enabled]);

  return connected;
}
