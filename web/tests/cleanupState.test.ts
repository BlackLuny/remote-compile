import assert from "node:assert/strict";
import test from "node:test";
import { resolveInitialCleanupView } from "../src/lib/cleanupState.ts";

test("reopening while cleanup is still running resumes polling active request", () => {
  const view = resolveInitialCleanupView({
    active: {
      ok: false,
      status: "running",
      request_id: "req-active",
      reclaimed: 0,
      skipped_active: 0,
      skipped_fresh: 0,
      disk_free_gb_before: 0,
      disk_free_gb_after: 0,
      reclaimed_worktrees: [],
      message: "",
    },
    latest: null,
  });

  assert.equal(view.activeRequestId, "req-active");
  assert.equal(view.result, null);
  assert.equal(view.error, null);
});

test("reopening after background cleanup completed shows latest result", () => {
  const view = resolveInitialCleanupView({
    active: null,
    latest: {
      ok: true,
      status: "done",
      request_id: "req-done",
      reclaimed: 4,
      skipped_active: 1,
      skipped_fresh: 2,
      disk_free_gb_before: 100,
      disk_free_gb_after: 140,
      reclaimed_worktrees: ["wt-1", "wt-2"],
      message: "reclaimed 4 worktree cache(s)",
    },
  });

  assert.equal(view.activeRequestId, null);
  assert.equal(view.isHistoricalResult, true);
  assert.equal(view.error, null);
  assert.equal(view.result?.request_id, "req-done");
  assert.equal(view.result?.reclaimed, 4);
});

test("reopening after background cleanup failed shows error and leaves form ready to retry", () => {
  const view = resolveInitialCleanupView({
    active: null,
    latest: {
      ok: false,
      status: "failed",
      request_id: "req-fail",
      reclaimed: 0,
      skipped_active: 0,
      skipped_fresh: 0,
      disk_free_gb_before: 0,
      disk_free_gb_after: 0,
      reclaimed_worktrees: [],
      message: "worker disconnected during cleanup",
    },
  });

  assert.equal(view.activeRequestId, null);
  assert.equal(view.result, null);
  assert.equal(view.isHistoricalResult, false);
  assert.match(view.error ?? "", /worker disconnected during cleanup/);
});

test("active running cleanup takes precedence over historical latest", () => {
  const view = resolveInitialCleanupView({
    active: {
      ok: false,
      status: "running",
      request_id: "req-new",
      reclaimed: 0,
      skipped_active: 0,
      skipped_fresh: 0,
      disk_free_gb_before: 0,
      disk_free_gb_after: 0,
      reclaimed_worktrees: [],
      message: "",
    },
    latest: {
      ok: true,
      status: "done",
      request_id: "req-old",
      reclaimed: 2,
      skipped_active: 0,
      skipped_fresh: 0,
      disk_free_gb_before: 100,
      disk_free_gb_after: 120,
      reclaimed_worktrees: ["wt-old"],
      message: "old pass",
    },
  });

  assert.equal(view.activeRequestId, "req-new");
  assert.equal(view.result, null);
  assert.equal(view.isHistoricalResult, false);
  assert.equal(view.error, null);
});

test("empty summary renders clean initial form", () => {
  const view = resolveInitialCleanupView({ active: null, latest: null });
  assert.deepEqual(view, {
    activeRequestId: null,
    result: null,
    isHistoricalResult: false,
    error: null,
  });
});
