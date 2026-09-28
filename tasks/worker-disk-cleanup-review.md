# Code review: manual worker disk cleanup

## Context

We added an admin-triggered manual disk cleanup for remote-compile workers.

**Requirements (must verify):**
1. Admin can click cleanup and run reclaim on one worker.
2. Modes: (a) idle older than N days, (b) all currently unused worktree caches.
3. During cleanup, stop accepting new tasks (temporary drain).
4. Never reclaim worktrees that currently have running tasks.
5. Do not clear project registry / git mirrors (only worktree target volumes + workspace dirs).
6. No commit / no deploy needed; review code only.

## Changed files

```
crates/rc-core/proto/rc.proto
crates/rc-server/src/admin.rs
crates/rc-server/src/app.rs
crates/rc-server/src/grpc_worker.rs
crates/rc-worker/src/client.rs
crates/rc-worker/src/main.rs
crates/rc-worker/src/runner.rs
web/src/api.ts
web/src/pages/WorkerDetail.tsx
web/src/pages/Workers.tsx
```

## Protocol

- `ServerCmd.cleanup` / `CleanupOrder` (`request_id`, `all_unused`, `idle_days`)
- `WorkerEvent.cleanup_done` / `CleanupDone` (reclaimed counts, disk before/after, skipped active/fresh, worktree ids)

## Flow

1. `POST /api/workers/{id}/cleanup` (admin) with `{all_unused?: bool, idle_days?: number}`
2. Server sets worker status draining, registers oneshot waiter, sends CleanupOrder
3. Worker sets local draining, runs `gc_with`, protects active worktree ids, restores draining if it was not previously draining, emits CleanupDone
4. Server returns JSON result; restores online if prior status was online

## Tests already green

- `cargo test -p rc-worker --tests` (includes `gc_protects_active_worktrees_even_when_all_unused`, `gc_idle_days_keeps_recent_caches`)
- `cargo test -p rc-server --bins`
- `web` `tsc --noEmit`

## Your job (review only — do NOT modify code)

Read the changed files thoroughly. Produce a structured review:

1. **Verdict**: Approve / Approve with nits / Request changes
2. **Correctness**: races, drain restore, protect set, double-cleanup, timeout cleanup of waiters
3. **Security / safety**: admin-only, cannot delete active task volumes, path safety
4. **API / UX**: REST body, error cases, UI modes
5. **Missing tests / edge cases**
6. **Findings** as a list with severity: Blocker / Major / Minor / Nit
   - Each finding: title, file:line if possible, why it matters, suggested fix

Write the full review to:
`.acp-agent/reviews/worker-disk-cleanup-REVIEW-<YOUR_MODEL_SLUG>.md`

Where YOUR_MODEL_SLUG is a short safe name of your model (e.g. `glm-5.2` or `deepseek-v4-flash`).

Then print a short summary in the chat (verdict + top findings only).
