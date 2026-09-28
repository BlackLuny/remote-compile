# Review: Manual worker disk cleanup

- Reviewer model: `deepseek-v4-flash`
- Date: 2026-08-12
- Scope: review only — no code modified
- Files: `crates/rc-core/proto/rc.proto`, `crates/rc-server/{admin,app,grpc_worker}.rs`, `crates/rc-worker/{client,main,runner}.rs`, `web/src/{api.ts, pages/WorkerDetail.tsx, pages/Workers.tsx}`

## Verdict: Request changes

The design is sound and the steady-state invariants hold, but two real races
can reclaim the worktree of a task the worker has *accepted* (requirement 4).
Both have small, clearly-specified fixes. Everything else is solid.

---

## 1. Requirements checklist

| # | Requirement | Status | Notes |
|---|-------------|--------|-------|
| 1 | Admin click → reclaim on one worker | ✅ | `POST /api/workers/{id}/cleanup` (admin.rs:453), admin-only via `AdminUser` extractor (403 for viewers) |
| 2 | Modes: idle > N days / all unused | ✅ | `all_unused` + `idle_days` (runner.rs:855-880); UI radio modes on both pages |
| 3 | Stop accepting new tasks during cleanup | ⚠️ | Worker-side `draining` flag refuses new Assigns (main.rs:281-290). Server-side status pin is clobbered by heartbeats for up to ~2s (finding M-5). Worker-side check holds — mostly. |
| 4 | Never reclaim worktrees with running tasks | ⚠️ | Steady state OK (protect set). Two races open the door (findings B-1, B-2) |
| 5 | Only worktree target volumes + workspace dirs | ✅ | `gc_with` skips volumes without `LABEL_WORKTREE` (runner.rs:822-823); registry/rustup volumes and mirror dirs untouched |
| 6 | Review only, no commit/deploy | ✅ | No changes made |

---

## 2. Correctness

### Good
- **Drain restore on error paths is correct.** `run_cleanup` restores the
  local flag before matching on the gc result, so a failed pass leaves the
  worker online (main.rs:382-384). Server restores status on send failure,
  waiter-drop and timeout (admin.rs:495-516).
- **Waiter lifecycle is complete.** `register_cleanup_wait` / `cancel_cleanup_wait`
  / `complete_cleanup` are called on every exit path; a late `CleanupDone`
  after timeout is a no-op (app.rs:153-175). No unbounded growth of
  `pending_cleanups`.
- **Heartbeat reconciliation** (`workers.rs:108`) already bounds the server's
  `assigned` set, so a slot claimed by a task that never started won't strand
  the worker.
- **Protect semantics** are derived from a task→worktree map keyed at the
  worker, which matches the volume label (`rc.worktree`) that GC keys off —
  consistent identifier space.
- **Idle-mode cutoff** uses `saturating_sub`/`saturating_mul` (runner.rs:870-872),
  so pathological `idle_days` values can't overflow.
- **Race with the hourly GC** is benign: two concurrent passes on the same
  volume just lose one delete (the loser fails `remove_volume` and is not
  double-counted).

### Problems

**B-1 — Protect-set TOCTOU: accepted-but-not-started tasks are not protected.**
The worker inserts into `active` *after* acquiring the semaphore, inside the
spawned task (main.rs:297-300). `run_cleanup` swaps `draining` and then
snapshots `protect` (main.rs:374). Sequence:
1. `Assign` received, draining check passes, task spawned — but it is parked on
   `permits.acquire()` (this window is **unbounded** on a saturated worker).
2. `CleanupOrder` arrives; `draining` is swapped; `protect` snapshot does not
   contain the queued task.
3. GC deletes that worktree's target volume and workspace dir.
4. The queued task acquires a permit, inserts into `active`, and starts —
   recreating the volume cold, or racing the delete mid-sync
   (`create_dir_all`/writes vs `remove_dir_all`, runner.rs:135/839) →
   transient IO failure → task retried elsewhere.

This violates requirement 4 in exactly the scenario where cleanup is most
useful (a disk-starved, busy worker). **Fix:** insert into `active` in the
command loop before `tokio::spawn` (remove happens in the spawned task as
today), or re-snapshot `protect` under the `active` lock immediately before
each delete inside `gc_with`.

**B-2 — Cleanup task outlives the session after a channel drop.**
The cleanup is `tokio::spawn`-ed detached (main.rs:325-328) and there is no
abort on session exit (contrast: heartbeat/gc are aborted, main.rs:355-357).
If the gRPC channel breaks mid-pass (server restart, network blip), the
session returns, `run()` reconnects with a **fresh empty `active` map and a
fresh `draining=false`**, while the old pass keeps deleting volumes from its
stale snapshot. Newly assigned tasks on the new session are invisible to the
old protect set → their worktrees can be reclaimed, and their `CleanupDone`
goes nowhere (server times out at 300 s and restores the status, which is
correct but masks the deleted-cache churn). **Fix:** keep the cleanup
`JoinHandle` and abort it on both session-exit paths (like heartbeat/gc), or
hoist `draining`/`active` to process scope.

**B-3 — Double cleanup on the same worker.** Two overlapping
`CleanupOrder`s (two admins, or retry after the 300 s timeout while the first
pass is still running) both swap `draining`; the first one to finish restores
the flag to false while the second pass is still deleting (main.rs:382-384),
reopening the drain window mid-pass. **Fix:** serialize with a worker-local
`Mutex<()>` around `run_cleanup`, or make `draining` a counter.

---

## 3. Security / safety

- **Admin-only:** enforced server-side by `AdminUser` (admin.rs:90-106), not
  just hidden buttons. Viewers get 403. Both UI surfaces gate on
  `role === "admin"` (Workers.tsx:138, WorkerDetail.tsx:126). ✅
- **Active volumes are never force-deleted:** `remove_volume` with
  `force: true` still fails when a container has the volume mounted
  (docker.rs:681-686), so a running build's mounted target volume is safe even
  if the protect set failed. The host workspace dir is only deleted
  *after* a successful volume removal (runner.rs:839-840) — but that same
  conditional means the workspace dir is the one piece that can be removed
  under an accepted task that has not mounted anything yet (B-1).
- **Path safety:** worktree ids are validated
  (`is_valid_worktree_id`, runner.rs:125-127) before becoming paths; GC reads
  ids from Docker labels this worker itself wrote; `our_volumes` filters by
  owner label (docker.rs:662-670). No attacker-controlled path components.
- **Request-id secrecy** prevents cross-worker waiter forgery: the server only
  hands `request_id` to the targeted worker. Not keying the waiter by
  `worker_id` is therefore safe today (see nit N-1).
- **Disk-space guard:** the scheduler's `InsufficientDisk` check is unrelated
  here; an admin can still drain a worker to near-zero disk. Expected for an
  explicit admin action; the UI shows disk before/after. Not a finding.

---

## 4. API / UX

- **REST body:** `{all_unused?: bool, idle_days?: number}` with `idle_days`
  defaulting to 14 (admin.rs:433-445) — sensible; negative values rejected.
- **Error cases:** not-connected → 400; waiter dropped / 300 s timeout →
  504 with drain restore; worker-side failure → 500 with the worker's message.
  All restore the prior status. ✅
- **UI:** both pages gate on admin, disable while pending, block modal close
  mid-flight, and show a readable result card (reclaimed / skipped / disk
  delta / worktree list). `api.ts` types match the JSON contract.
- **UI hazard (minor):** in *idle* mode an empty or `0` input silently means
  `idle_days = 0` (`Math.floor(Number(idleDays) || 0)`), which reclaims
  *everything* not in use — the exact opposite of the "keep recent caches"
  intent. WorkerDetail.tsx has no NaN guard at all; CleanupModal's
  `Number.isNaN` guard doesn't catch `""` (which is `0`). The server accepts
  `idle_days = 0` for the non-`all_unused` mode, and the proto comment
  ("0 means idle since epoch … only timestamp 0") is wrong: with cutoff = now,
  everything unprotect-ed is reclaimed. Fix: reject `idle_days == 0` unless
  `all_unused`, and have the UI treat empty input as the default (14).

---

## 5. Missing tests / edge cases

- **Server:** no tests for `cleanup_worker` (not-connected, send-failure
  restore, timeout restore, waiter-cancel) nor for the new
  `App::register/complete/cancel_cleanup_wait` machinery. `--bins` only proves
  it compiles.
- **Worker:** no test for `run_cleanup` drain-flag restore on the *error*
  path of `gc_with` (the `was_draining` restore is right, but untested), and
  no test pinning the B-1 ordering (accepted-but-queued task must be in the
  protect set). `decide_reclaim` unit tests are good; the drain-vs-assign
  ordering in the command loop is the important untested invariant.
- **Edge cases verified by inspection:** concurrent hourly GC + manual
  cleanup (benign); `idle_days` overflow (saturated); missing workspace dir
  (last_used = 0, reclaimed); old-worker compatibility — untested, see M-6.

---

## 6. Findings

### Blocker
None.

### Major

**B-1. Protect-set TOCTOU: queued (accepted, not-yet-started) tasks can be reclaimed**
- `crates/rc-worker/src/main.rs:297-300` (insert after `permits.acquire()`), `main.rs:374` (snapshot), `runner.rs:821-847` (delete)
- Why: requirement 4 is violated in the exact scenario cleanup targets — a
  saturated worker where new assigns park on the semaphore for minutes. The
  task then runs cold or fails mid-sync.
- Fix: insert `task_id → worktree_id` into `active` synchronously in the
  command loop before spawning (remove stays in the spawned task), or
  re-verify against `active` under the lock before each deletion.

**B-2. Cleanup task survives a session drop with a stale protect set and drain state**
- `crates/rc-worker/src/main.rs:325-328`, `main.rs:355-357`
- Why: on channel failure the session reconnects with a fresh empty `active`
  and `draining=false` while the detached pass keeps deleting; new tasks can
  lose their caches and the server 504s.
- Fix: abort the cleanup `JoinHandle` on session exit (same pattern as
  heartbeat/gc), or scope `active`/`draining` at the process level.

### Minor

**M-3. Double cleanup: first pass restores `draining` while the second is still deleting**
- `crates/rc-worker/src/main.rs:372-384`
- Why: overlapping orders (two admins, or a UI retry after the 300 s timeout)
  reopen the accept-new-work window mid-deletion.
- Fix: worker-local `Mutex<()>` serializing `run_cleanup`, or a draining counter.

**M-4. `idle_days = 0` silently means "reclaim everything" — UI can trip it with an empty input**
- `crates/rc-server/src/admin.rs:459-460`, `web/src/pages/WorkerDetail.tsx` (idle-mode mutate), proto comment on `CleanupOrder.idle_days`
- Why: an operator typing "cleanup idle caches" with an empty/0 field wipes
  recent caches too; the proto comment describing 0 as "only timestamp 0"
  is inaccurate (cutoff = now ⇒ everything).
- Fix: reject `idle_days == 0` unless `all_unused`; default the empty UI input to 14.

**M-5. Heartbeat clobbers the server-side drain pin between `set_status` and the worker's flag swap**
- `crates/rc-server/src/workers.rs:91-110` (`heartbeat` overwrites status), `crates/rc-server/src/admin.rs:479-483`
- Why: for up to ~2 s after the order is sent, heartbeats report "online" and
  the scheduler may assign; the worker's local flag refuses the task and
  bounces an infra-failure round trip (churn, not corruption).
- Fix: set the in-memory status *after* the send succeeds (or accept and
  document; the worker-side check is the real gate).

**M-6. No version/capability gate on `CleanupOrder`**
- `crates/rc-server/src/admin.rs:484-492`
- Why: a pre-upgrade worker drops the unknown oneof body and never replies —
  the admin waits the full 300 s for a 504. Mixed-version fleets are the norm
  here.
- Fix: reject the request when `worker.version` predates cleanup support (and
  maybe reduce the timeout), or expose a `cleanup` capability.

### Nit

**N-1. `complete_cleanup` keys only by `request_id`, not `(worker_id, request_id)`**
- `crates/rc-server/src/app.rs:165-170`, `grpc_worker.rs:279-281`
- Safe today (the id is only known to the addressed worker), but scoping by
  worker is one line and makes the invariant self-evident.

**N-2. Test coverage gap** — see §5: nothing exercises the server waiter
paths (timeout/cancel/restore) or `run_cleanup`'s error-path drain restore;
no test pins the B-1 ordering.

**N-3. `reclaimed_worktrees` echoed in full** in the JSON response
(admin.rs:553) while the UI truncates at 16 chars — fine, but consider
truncating server-side for very large passes (JSON payload size).

---

## 7. Summary

Approve after fixing B-1 (insert into `active` before spawn) and B-2 (abort
cleanup on session exit); both are small and local. M-3/M-4 are worth folding
into the same pass. The core invariants — admin-only, registry/mirror
preservation, drain restore on every error path, waiter cleanup — are all
implemented correctly.
