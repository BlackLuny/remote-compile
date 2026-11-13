import type { WorkerCleanupResult, WorkerCleanupSummary } from "../api.ts";

export interface CleanupModalView {
  activeRequestId: string | null;
  result: WorkerCleanupResult | null;
  isHistoricalResult: boolean;
  error: string | null;
}

export function resolveInitialCleanupView(summary: WorkerCleanupSummary): CleanupModalView {
  if (summary.active && summary.active.status === "running") {
    return {
      activeRequestId: summary.active.request_id,
      result: null,
      isHistoricalResult: false,
      error: null,
    };
  }
  if (summary.latest) {
    if (summary.latest.status === "done") {
      return {
        activeRequestId: null,
        result: summary.latest,
        isHistoricalResult: true,
        error: null,
      };
    }
    if (summary.latest.status === "failed") {
      return {
        activeRequestId: null,
        result: null,
        isHistoricalResult: false,
        error: `上次后台清理失败：${summary.latest.message || "未知错误"}`,
      };
    }
  }
  return {
    activeRequestId: null,
    result: null,
    isHistoricalResult: false,
    error: null,
  };
}
