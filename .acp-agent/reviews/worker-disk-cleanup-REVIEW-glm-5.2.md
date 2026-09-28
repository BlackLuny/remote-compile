# Review: manual worker disk cleanup

- Reviewer model: `glm-5.2`
- Date: 2026-08-12
- Scope: code review only — no code modified
- Baseline: `HEAD` of `main` (commit `355fefa`)
- Files reviewed: `crates/rc-core/proto/rc.proto`, `crates/rc-server/{admin,app,grpc_worker}.rs`, `crates/rc-worker/{client,main,runner}.rs`, `web/src/{api.ts, pages/WorkerDetail.tsx, pages/Workers.tsx}`
- Verification: `cargo test -p rc-worker --tests` (82 ok), `cargo test -p rc-server --bins` (130 ok), `web` `npx tsc --noEmit` clean, `cargo check -p rc-server -p rc-worker` clean (only pre-existing dead-code warnings).

## Verdict: Request changes

The design is solid and steady-state invariants hold, but two distinct races can
delete a worktree volume underneath a task the worker has already accepted, which
is exactly the invariant requirement 4 promises. Both have small, well-scoped
fixes. The rest of the patch is well-built and the test coverage is honest.

---

## 1. Requirements checklist

| # | Requirement | Status | Notes |
|---|-------------|--------|-------|
| 1 | Admin click → reclaim on one worker | OK | `POST /api/workers/{id}/cleanup` (`crates/rc-server/src/admin.rs:453`), gated by `AdminUser` extractor → 403 for viewers |
| 2 | Modes: idle > N days / all unused | OK | `all_unused` + `idle_days` on the wire (`rc.proto`), decoded in `decide_reclaim` (`runner.rs:873`); UI surfaces both modes on both pages |
| 3 | Stop accepting new tasks during cleanup | Partial | Worker-side `draining` flag refuses Assign (`main.rs:281`). Server-side `set_status("draining")` is overridden by the next heartbeat for up to ~2 s (finding **B-2**). Worker-side gate is the load-bearing one and holds — except in the concurrent-cleanup case (**B-1**) where the second cleanup sees `was_draining=true` and does not re-arm. |
| 4 | Never reclaim active worktrees | Partial | `protect = active.values()` (`main.rs:374`) and `decide_reclaim` short-circuits on it (`runner.rs:881`). Two races bypass it: concurrent cleanups (**B-1**) and the assign-after-cleanup-snapshot window (**B-2**). |
| 5 | Only worktree target volumes + workspace dirs | OK | `gc_with` skips any volume lacking `LABEL_WORKTREE` (`runner.rs:822`); registry/rustup volumes have only `LABEL_PROJECT`/base labels, so they are untouched. Mirror dirs (`mirror_dir()`) are never touched here. |
| 6 | Review only, no commit/deploy | OK | No commits made. |

---

## 2. Correctness

### B-1  Concurrent cleanups on the same worker can unwind the drain too early (Blocker)

`crates/rc-worker/src/main.rs:319-329` spawns `run_cleanup` with no mutual exclusion. Two concurrent `CleanupOrder`s (two admins, or one admin double-clicking before the request returns) race on the `draining` flag:

```
T0  cleanup1.swap(true) → was_draining1 = false
T1  cleanup2.swap(true) → was_draining2 = true
... both run gc_with concurrently, both may delete volumes ...
T2  cleanup1 done, was_draining1 == false → draining.store(false)
T3  cleanup2 done, was_draining2 == true  → does NOT touch draining
```

At T2 the worker starts accepting `Assign`s again, but cleanup2 is **still running**
and may call `remove_volume` on a worktree that a freshly-accepted task just
started using. This is the precise "delete volume under a running task" scenario
requirement 4 forbids. The `protect` snapshot is taken once at the start of each
cleanup, so the new task is not in cleanup2's `protect`.

Why it matters: an admin double-click (easy to do — the UI button has no debounce
on the list page; see **N-3**) can corrupt a build. The recovery is "task fails
with infra error and retries elsewhere", which is what the design tries to avoid.

Suggested fix: serialise cleanups per worker with a `tokio::sync::Mutex<()>` (or
an `AtomicBool` "cleanup in progress" that returns `CleanupDone{ok:false}` if
already running). The simplest is a per-worker mutex taken in the command
handler before spawning, or a single `Mutex<()>` in `Runner` since cleanups are
already rare.

### B-2  Heartbeat clobbers the server-side "draining" pin (Blocker)

`crates/rc-server/src/grpc_worker.rs:233-234` runs on every heartbeat:

```rust
app.workers.heartbeat(worker_id, stats.clone(), &hb.status, ...);
app.store.touch_worker(worker_id, &hb.status).ok();
```

and `WorkerRegistry::heartbeat` (`workers.rs:102`) does `w.status = status.to_string()`,
unconditionally overwriting the value the admin just set.

Sequence:

```
T0  admin: app.workers.set_status(id, "draining")
T1  worker heartbeat (local draining still false) → hb.status = "online"
    → server w.status = "online"   ← the pin is gone
T2  admin sends CleanupOrder; worker eventually swaps local draining = true
T3  next heartbeat → server w.status = "draining" again
```

Between T1 and T3 the scheduler (`scheduler::evaluate`, `scheduler.rs:67`) sees
`status == "online"` and may dispatch a new `Assign`. The worker has not yet
swapped its local `draining` flag, so the Assign is accepted. The new task's
worktree is **not** in the cleanup's `protect` snapshot (which is taken inside
`run_cleanup`, after the swap). If the cleanup reaches `remove_volume` for that
worktree before the new task's spawn reaches `active.lock().insert(...)`, the
volume is deleted under the task.

This is the same race that already affects the existing `drain_worker` route
(`admin.rs:407-419`), but for `drain_worker` the worst case is "one extra task
runs before the drain takes effect" — fine. For cleanup, the worst case is
"task's volume is removed under it" — not fine.

Why it matters: requirement 4 is violated in a narrow but reachable window.

Suggested fix (any one of):
- Server-side: don't let a worker's heartbeat downgrade a status the control
  plane set administratively. Track an `admin_pinned: bool` (or compare: never
  let a heartbeat move status away from `"draining"` unless the worker reports
  `"draining"` itself). Apply the same rule to `drain_worker` while you're there.
- Or: have the worker flip its local `draining` flag the moment it **receives**
  `CleanupOrder`, before spawning `run_cleanup` — i.e. do the `swap(true)`
  inline in the command loop, then pass `was_draining` into the spawned task.
  This shrinks the window to the time between command receipt and the next
  heartbeat, which is still > 0 but much smaller. Combined with the server-side
  pin above it becomes airtight.
- Or: take `active.lock()` and the `draining` swap atomically with the
  `Assign` path — e.g. the Assign branch also locks `active` and re-checks
  `draining` under that lock, so an Assign that arrives after cleanup's snapshot
  is either blocked by `draining` or seen by the snapshot.

### M-3  Slow cleanup past 300 s leaves the worker "draining" with no admin escape (Major)

`admin.rs:500-518`: on `tokio::time::timeout` the server calls
`restore_worker_status(&app, &id, &prior_status)` and returns `GATEWAY_TIMEOUT`.
But the worker is still running cleanup, its local `draining == true`, and every
2 s the heartbeat re-asserts `w.status = "draining"` on the server, overriding
the restore. So after the timeout:

- The admin HTTP call has returned (good — no hung request).
- The worker is effectively still draining (correct — it really is busy).
- The admin cannot `resume_worker` the machine: a `resume` sets `w.status="online"`,
  but the next heartbeat (status="draining") overwrites it again.

The machine is wedged in "draining" until the worker's `gc_with` actually
finishes. If `gc_with` is hung (e.g. Docker daemon unresponsive), the machine is
wedged indefinitely with no admin recovery short of killing the worker process.

Why it matters: a 300 s timeout that strands a machine is worse than not having
the timeout. The timeout currently protects the HTTP request, not the worker.

Suggested fix: when the timeout fires, send `Drain(false)`-equivalent? There is
no such message. Cheapest: when the timeout fires, set a server-side flag
`cleanup_timed_out[id] = true` and have `handle_event` ignore further
heartbeats from that worker until a `CleanupDone` arrives (or N seconds pass),
then restore. Alternatively, expose a `force_resume` that flips a server-side
`admin_pinned` flag and ignores heartbeat status until the next admin action.
This is the same class of bug as **B-2** — the heartbeat wins.

### M-4  `restore_worker_status` runs even when worker self-reported failure (Minor → ok)

`admin.rs:519-526`: the status restore happens before the `if !done.ok` check
(line 540), so a failed cleanup still puts the worker back to "online". This is
correct (a failed gc is not a reason to strand the machine) but worth noting
that `done.ok == false` returns `INTERNAL_SERVER_ERROR` **after** the audit log
and status restore — so the audit row is written even on failure. Good.

### M-5  Audit logs `body.idle_days`, not the clamped value (Nit)

`admin.rs:534-536` records `body.idle_days` (raw user input). When
`all_unused=true && body.idle_days=-5`, the validation at line 459 does not
fire (the `&&` short-circuits on `all_unused`), so the order is sent with
`idle_days=0` (clamped at line 475) but the audit shows `idle_days=-5`. Cosmetic,
but misleading.

Suggested fix: audit `order.idle_days` (the clamped value) instead.

### M-6  Disk-after is read before the worker has actually freed the space (Nit)

`main.rs:385` reads `disk_after = sysinfo::disk_free_gb(&cfg.data_dir)` immediately
after `gc_with` returns. `remove_volume` is async on the Docker side and the
filesystem may still be flushing inode cleanup. The `disk_after` in the result
can be slightly understated. Not a correctness issue — the value is
informational — but worth a comment so operators don't chase a "reclaimed 10 but
disk only went up by 3" surprise.

---

## 3. Security / safety

### S-1  Admin-only (OK)

`admin.rs:453` uses `AdminUser(u): AdminUser`, which the extractor (`admin.rs:90-105`)
rejects with `403 FORBIDDEN` for non-admin sessions. Viewers cannot trigger
cleanup. Good.

### S-2  Active task volumes cannot be deleted in steady state (OK with races)

`decide_reclaim` (`runner.rs:873-882`) checks `protect.contains(worktree)` first.
`protect = active.lock().values().cloned().collect()` (`main.rs:374`). In steady
state (no concurrent cleanup, no heartbeat race) this is correct. The races in
**B-1** and **B-2** are the gaps.

### S-3  Path safety on `workspace_dir` (OK, pre-existing property)

`workspace_dir(work_root, worktree)` is `work_root.join(worktree)`
(`docker.rs:763-764`). `Path::join` does not sanitise `..`, so a malicious
`worktree` label could escape `work_root`. Mitigation: the `worktree` label is
set by `Runner` itself from `assignment.worktree_id`, which is validated at
admission time via `ids::is_valid_worktree_id` (`app.rs:293` — `w-` + 16 hex
digits, see `ids.rs:63-68`). So a normal worktree label can never contain `/`
or `..`. The risk is only if an attacker has direct Docker socket access on the
worker host and can hand-craft a volume with `LABEL_WORKTREE=../../etc` — at
which point they already own the machine. This is a pre-existing property of
the hourly `gc` and is not made worse by this patch, so I am not raising it as a
finding, only recording it.

### S-4  No project registry / git mirror deletion (OK)

`gc_with` iterates `our_volumes()` and `continue`s on any volume without a
`LABEL_WORKTREE` label (`runner.rs:822-823`). Registry volumes
(`registry_volume(project_id)` = `rc-cargo-{project_id}`) are created with only
`LABEL_PROJECT` + base labels (`runner.rs:387-392`), so they are skipped. Rustup
volume (`rc-rustup`) has only base labels, also skipped. Git mirrors live under
`mirror_dir()`, which `gc_with` never touches. Requirement 5 satisfied.

### S-5  `request_id` collision resistance (OK)

`ids::random_token()` is two concatenated ULIDs (`ids.rs:114-118`), 52 chars,
~212 bits of entropy. Collision between two concurrent cleanups on the same
worker is effectively impossible, so `pending_cleanups` lookup is safe.

---

## 4. API / UX

### A-1  REST body and error cases (OK)

`POST /api/workers/{id}/cleanup` with JSON `{ all_unused?: bool, idle_days?: number }`.
Defaults: `all_unused=false`, `idle_days=14` (`admin.rs:435-445`). Validation:
`!all_unused && idle_days < 0` → 400. Disconnected worker → 400. Send failure →
400. Timeout → 504. Worker-reported failure → 500 with `done.message`. Success →
200 with the full `WorkerCleanupResult` shape. The error shape matches the
frontend's `ApiError` parser (`api.ts:20-28`), which surfaces `body.error`.

One gap: on 500 (worker `done.ok=false`) the response body is just the error
string; the `reclaimed`/`skipped_*`/`disk_*` fields from `CleanupDone` are
dropped. The operator sees "reclaimed 5" in the audit log but the UI only shows
the error message. Minor UX gap.

### A-2  `idle_days` semantics when `all_unused=true` (Nit)

`rc.proto` says `idle_days` is "used when `all_unused=false`" and recommends
`all_unused`. The server clamps `idle_days` to `>= 0` and forwards it regardless.
`decide_reclaim` only consults `idle_days` when `all_unused=false` (`runner.rs:882-886`),
so sending `idle_days` with `all_unused=true` is harmless. The UI sends
`idle_days=0` in that case (`WorkerDetail.tsx:303-308`, `Workers.tsx` `onRun`),
which is fine. Document the ignored field more loudly in the proto comment if
you care.

### A-3  UI: list page has no disable while a cleanup is in flight (Nit → see N-3)

On `Workers.tsx`, the "清理磁盘" button in the row is not disabled when
`cleanup.isPending` — only the buttons inside the modal are. An admin can
click "清理磁盘" on a different worker (or the same one — see **B-1**) while
a cleanup is running, opening a second modal. The mutation's `isPending` is
shared across all workers, so the second modal's "开始清理" button is disabled
while the first is running, which prevents the worst case — but the modal can
still be opened with stale state. See **N-3**.

### A-4  UI: WorkerDetail does not show `reclaimed_worktrees` (Minor)

`WorkerDetail.tsx:282-290` renders the summary line but omits the
`reclaimed_worktrees` list that `Workers.tsx:319-329` shows. Inconsistent.
Suggested fix: port the `reclaimed_worktrees` block from `Workers.tsx`.

### A-5  UI: cleanup result is not cleared when the admin changes mode (Nit)

After a successful cleanup, `WorkerDetail.tsx` shows the result block. The
modal's "关闭" button calls `setCleanupResult(null); cleanup.reset();` (line
296-297), which is correct. But if the admin closes the modal and reopens it
via "清理磁盘" (line 130-131), the same `setCleanupResult(null); cleanup.reset()`
runs. Good — no stale result. This is fine; recorded for completeness.

---

## 5. Missing tests / edge cases

### T-1  No test for the concurrent-cleanup race (B-1)

The two unit tests (`gc_protects_active_worktrees_even_when_all_unused`,
`gc_idle_days_keeps_recent_caches`) cover the pure `decide_reclaim` logic, which
is correct. There is no test that two concurrent `run_cleanup` calls on the same
worker do not unwind each other's drain. Suggested: a unit test that runs two
`run_cleanup`s with a stubbed `gc_with` that sleeps, and asserts `draining` is
still true after the first finishes-while-the-second-runs.

### T-2  No test for the heartbeat-clobbers-drain race (B-2)

`workers.rs` tests cover `set_status`, `heartbeat`, `disconnect`, but not the
interaction where an admin sets `draining` and a subsequent heartbeat with
`status="online"` overwrites it. Suggested: a test that calls `set_status("draining")`
then `heartbeat(..., "online", ...)` and asserts the registry still reports
`"draining"` (after the fix).

### T-3  No integration test for the full round-trip

The control plane registers a waiter, the worker emits `CleanupDone`,
`complete_cleanup` delivers it, the HTTP handler returns 200. There is no test
exercising `register_cleanup_wait` → `complete_cleanup` → receiver gets the
struct. Suggested: a unit test on `App` that registers a waiter and completes it
with a fake `CleanupDone`, asserting the receiver resolves.

### T-4  No test for `idle_days == 0` with `all_unused == false`

`decide_reclaim` with `idle_days=0` and `last_used_secs > now_secs` (clock skew,
or a file modified in the future) would skip as "fresh" because
`last_used_secs > cutoff = now_secs`. The proto comment says "0 means idle
since epoch (effectively all unused that still have a last-used timestamp of
0)", but the code treats `idle_days=0` as "cutoff = now" → anything with
`last_used > 0` is fresh. This contradicts the proto comment.

Reading `runner.rs:883-886`:
```rust
let idle_days = policy.idle_days.max(0);
let cutoff = now_secs.saturating_sub(idle_days.saturating_mul(24 * 3600));
if last_used_secs > cutoff { SkipFresh } else { Reclaim }
```

With `idle_days=0`, `cutoff = now_secs`, so `last_used_secs > now_secs` is
almost always false (last-used is in the past), so it reclaims. Wait —
`last_used_secs > cutoff` means "last used is more recent than cutoff". If
`cutoff == now`, then `last_used_secs > now` is false for any past timestamp, so
everything reclaims. That contradicts what I said. Let me re-trace:
- `idle_days=0`, `now=1000`, `cutoff = 1000 - 0 = 1000`.
- File last used at `900` (in the past): `900 > 1000` is false → `Reclaim`.
- File last used at `1000`: `1000 > 1000` is false → `Reclaim`.
- File last used at `1001` (future, clock skew): `1001 > 1000` → `SkipFresh`.

So `idle_days=0, all_unused=false` reclaims everything whose `last_used <= now`,
which is effectively "all unused" (modulo future-dated files). The proto comment
"0 means idle since epoch (effectively all unused that still have a last-used
timestamp of 0)" is **wrong** — `idle_days=0` is much more aggressive than that.
The behavior is reasonable (reclaim everything not in the future) but the
comment is misleading. Nit on the proto comment.

### T-5  No test for the timeout path

No test exercises the 300 s timeout, the `cancel_cleanup_wait` path, or the
"worker still running after timeout" interaction with heartbeats (**M-3**).
Hard to test without injecting a clock, but at least the `cancel_cleanup_wait`
helper could have a unit test asserting it drops the waiter cleanly.

---

## 6. Findings (severity-ordered)

### Blocker

- **B-1**  Concurrent `CleanupOrder`s on the same worker race on the `draining`
  flag (`crates/rc-worker/src/main.rs:319-329` + `361-384`). The first cleanup to
  finish resets `draining=false` while the second is still running, so the
  worker starts accepting Assigns whose worktree the second cleanup may then
  delete. Requirement 4 is violated. Fix: serialise cleanups per worker
  (mutex or "cleanup in progress" guard).

- **B-2**  Heartbeat unconditionally overwrites the admin-pinned `"draining"`
  status on the server (`crates/rc-server/src/grpc_worker.rs:233-234` +
  `workers.rs:102`). For up to ~2 s between the admin call and the worker
  processing the order, the scheduler can dispatch a new Assign to the worker,
  which the worker accepts (local `draining` still false). The new task's
  worktree is not in the cleanup's `protect` snapshot. Requirement 4 is
  violated in that window. Fix: server-side `admin_pinned`-style flag that
  survives a heartbeat, or flip the worker's local `draining` flag inline in
  the command handler (before spawn) so the next heartbeat reports `draining`.

### Major

- **M-3**  After the 300 s timeout the worker is wedged in `"draining"` by its
  own heartbeats, and the admin cannot `resume` it (`crates/rc-server/src/admin.rs:500-518`).
  Same root cause as B-2: heartbeat wins. Fix: same as B-2, plus a server-side
  override path for "force resume".

### Minor

- **M-4**  On worker-reported failure (`done.ok=false`), the response drops
  `reclaimed`/`skipped_*`/`disk_*`/`reclaimed_worktrees`, so the operator only
  sees the error string (`crates/rc-server/src/admin.rs:540-545`). The audit
  log has the counts. Consider returning the partial result with an `ok: false`
  flag instead of mapping to 500, or at least include the counts in the error
  body.
- **A-4**  `WorkerDetail.tsx` does not render `reclaimed_worktrees`, but
  `Workers.tsx` does. Inconsistent. Port the list block.
- **N-3**  `Workers.tsx` row-level "清理磁盘" button is not disabled while
  `cleanup.isPending`, so a second modal can be opened (the inner "开始清理"
  is disabled, which prevents the worst case). Disable the row button when
  `cleanup.isPending` for clarity.

### Nit

- **M-5**  Audit logs `body.idle_days` (raw, can be negative when
  `all_unused=true`); log `order.idle_days` (the clamped value) instead
  (`crates/rc-server/src/admin.rs:534`).
- **M-6**  `disk_after` is read immediately after `gc_with` returns; Docker may
  still be freeing inodes. Add a comment so operators don't chase the
  discrepancy (`crates/rc-worker/src/main.rs:385`).
- **A-2**  Proto comment for `CleanupOrder.idle_days` claims "0 means idle since
  epoch", but the code treats `idle_days=0` as "cutoff = now", which reclaims
  everything not in the future. Fix the comment (`rc.proto:555-557`).
- **T-4**  No test for `idle_days == 0` semantics; the behavior is reasonable
  but undocumented and diverges from the proto comment.

---

## 7. Things that are good

- `active` is now `HashMap<String, String>` (task_id → worktree_id) instead of a
  `HashSet<String>`; this is the right shape for the protect set and the
  reconcile against `hb.active_task_ids` (which is still a `Vec<String>` of
  task ids) still works because `active.lock().keys().cloned().collect()` keeps
  the task-id list semantics (`main.rs:224`). No regression.
- `GcPolicy` / `GcReport` / `decide_reclaim` factored out is clean and
  testable; the two unit tests cover the meaningful branches.
- Server-side waiter (`pending_cleanups`) is keyed by `request_id` and removed
  on all three exit paths (success, send-fail, timeout, receiver-drop). No
  leak path visible.
- Status restore happens before the `done.ok` check, so a failed cleanup does
  not strand the machine.
- Audit row is written for every cleanup call, success or failure, with the
  admin username and counts.
- `restore_worker_status` calls `dispatch_signal.notify_one()` when going back
  online, so the scheduler wakes immediately and the worker is not stuck
  waiting for the next dispatch tick.
- UI on both pages disables the "开始清理" button while pending and shows a
  "清理中…" label. Result block distinguishes ok from error.
- The `Workers.tsx` modal surfaces `reclaimed_worktrees` as `Mono` chips with
  truncation — nice operator UX.

---

## 8. Suggested fix order

1. B-2 (server-side admin pin that survives heartbeats) — this also fixes M-3
   and the pre-existing `drain_worker` race. Largest leverage.
2. B-1 (serialise cleanups per worker) — small, isolated change.
3. M-4 / A-4 / N-3 — UX polish, can ride a follow-up.
4. M-5 / M-6 / A-2 / T-4 — comments and a doc fix; bundle into one nit commit.

After B-1 and B-2 are fixed, the manual cleanup path satisfies all six
requirements. The rest is polish.