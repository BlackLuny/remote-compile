# Review: admin image registry mirror (push/pull)

Read-only review of the admin-driven push/pull feature. No fixes were made.
Scope: proto, `rc-server` (config/images/admin/grpc_worker), `rc-worker` (docker/runner/main), web (Settings/Images/api).

Severity key: **blocker** = ship-stopping correctness or security; **major** = real bug/security gap that should be fixed soon; **minor** = edge case or UX/API gap; **nit** = style/consistency.

---

## 1. Correctness bugs

### [blocker] Pull does not preserve the digest pin — `also_local_tag` can silently rebind the approved image to the wrong bytes
`images.rs:455-509` (`dispatch_pull`) sends `also_local_tag = row.image_ref` AND `local_ref = row.image_ref` to every worker. On the worker, `runner.rs:630-657`:
- `pull_and_tag(remote, Some(image_ref))` calls `ensure_image(remote)`. If the worker already has an image locally that matches `remote` (e.g. a **local-only build** named `rc-registry/env/<short>:latest`, whose tag string equals the row's `image_ref`), `ensure_image` returns it **without any pull** (`docker.rs:144-147`).
- If a pull *does* happen, `pull_and_tag` tags the pulled image as `image_ref` (local, floating tag).
- Then `mirror_image_inner` tags the pulled image with `local_ref` again.
- **Nothing on the worker verifies the pulled image's repo digest matches `row.digest`** (which the server has but does not send). The server's whole trust model is digest-only (`ResolvedProfile.image` MUST be a digest ref; `rc.proto:51`, `full_ref` at `images.rs:168-179` pins `@digest`). After a pull, existing task profiles pin `rc-registry/env/<short>@sha256:<old>` — the OLD digest — while the local floating tag now points at the new bytes. The first `docker pull` of the new digest fixes the local name, but until then the fleet can run a **different image than the approved digest**, or fail.

Root cause: digest reconciliation is missing from `ImageMirrorDone`. `ImageMirrorDone` (`rc.proto:597-603`) carries only `remote_ref`, no `local_ref`/digest, so `on_mirror_done` (`images.rs:547-579`) cannot verify or record what actually landed. Worker should return the repo digest it pulled, and the server should verify `digest == row.digest` and record it on the image row.

Also note `rc-worker/src/docker.rs:148-153`: `ensure_image`'s id fallback — `image.split_once('@')` then `inspect_image(id)` — resolves `repo@sha256:<content-id>` to a locally-built image by id. This is correct for the build path, but it makes "pull" of a `hub.covm.net/rc-env:0f5446c3` name succeed without pulling when the worker already has *any* local image tagged with that name, even a stale one.

### [major] Push local_ref sends the raw digest and tags it to a floating remote tag — remote is then not digest-pinned, and multi-arch is silently flattened
`images.rs:428-432`: for push, `local_ref = row.digest` (e.g. `sha256:abc…`). `runner.rs:607-628`: `ensure_image("sha256:abc…")` — resolves via the digest-as-name/id path; then `tag_image(resolved, repo, tag)` tags `hub.covm.net/rc-env:0f5446c3`. `push_image` then pushes **the tag** (floating). Anyone pulling later gets whatever the tag now points to; the tag is mutable and can be overwritten by a rebuild+repush with a different digest while a worker already cached the old one. Since remote refs are derived only from the env short id, a rebuild changes content but not the remote tag — stale pullers keep running the old image indefinitely (no digest-tagged push, e.g. `:0f5446c3@sha256:…`). This interacts with the digest-pin issue above. Also: the local worker may only have one platform of the image; pushing a tag is fine for that platform, but nothing records which arch was pushed (see §3/§5).

### [major] Push requires `local_ref` on the worker but `dispatch_push` never checks the worker actually has the image
`pick_worker_for_mirror(app, worker_id, /*prefer_has_image*/ true)` (`images.rs:528-545`) ignores the `prefer_has_image` flag entirely (parameter is `_prefer_has_image`), so it can pick the first online worker that has never built the env. `ensure_image(digest)` then triggers a `docker pull` of `sha256:abc…` from... Docker Hub (no registry host in the ref) — which fails or pulls an unrelated image. The "prefer the worker that has the image" intent is dead code; there is no worker-side "do you have this digest" capability in the heartbeat. Best outcome today: the push fails with a confusing error on a random worker.

### [minor] Push with a worker that only has the image under its local name fails
`dispatch_push` sends `local_ref = row.digest`. If the builder's image was never assigned a tag that resolves the digest (builds tag `rc-registry/env/<id>:latest` per `prepare_env`, and `digest_of` returns the content id), `ensure_image(digest)` may miss while the image exists under the local tag. Should fall back to `image_ref` (local tag) and/or a `docker inspect` by id.

### [minor] `split_repo_tag` mishandles registry host:port refs (used in push tag-splitting path)
`docker.rs:730-747`: for `localhost:5000/foo` it returns `(image, None)` (host part contains `:` and the split "tag" side contains `/`). That's actually the intended keep-whole behavior for the splitter, BUT the same logic is reimplemented ad hoc in `runner.rs:614-625` for push tag splitting: `hub.covm.net/rc-env:0f5446c3` → left `hub.covm.net/rc-env`, tag `0f5446c3` — OK; but a registry host with port (`hub.covm.net:5000/rc-env:0f5446c3`) → `rsplit_once(':')` gives left `hub.covm.net:5000/rc-env`, tag `0f5446c3` — actually still OK for this case. The real gap: `registry_credentials_for` (docker.rs:751-806) parses the registry host with `image.split_once('/')` — for `hub.covm.net/rc-env` that yields `hub.covm.net` — fine; but for a hostless name (`alpine`) it returns `None` (correct). The credential lookup also does a **prefix/substring match** (`starts_with(host) || k.contains(host)`, docker.rs:772-778) which can pick the wrong credential entry when one registry host is a prefix of another (e.g. `hub.covm.net` vs `hub.covm.net.evil.com`), and matches `https://hub.covm.net/v2/` only by `contains(host)`. Minor, but worth a comment + exact match on the normalized host.

### [minor] `pull_and_tag` tags with `"latest"` when `also_tag` has no tag
`docker.rs:209-217`: `split_repo_tag(full)` → if no tag, `tag.unwrap_or("latest")`. `row.image_ref` is always `rc-registry/env/<short>:latest` for built envs so it's usually fine; but for upstream-image envs (`image_ref` = upstream ref, `pull_ref` set, `prepare_env` at `images.rs:38-43`), `image_ref` could be `ubuntu:24.04`-style, and the `also_local_tag` becomes that — tagging the pulled remote image as `ubuntu:24.04`, i.e. **overwriting a standard local name for a possibly-different image**. Pulling `hub.covm.net/rc-env:X` onto a worker that has `ubuntu:24.04` cached and then tagging the pull as `ubuntu:24.04` changes what `ubuntu:24.04` resolves to locally. Digest pinning is what makes the original ref safe; this silently breaks it.

### [minor] `dispatch_pull` `also_local` doesn't respect the digest
`images.rs:468`: `let also_local = row.image_ref.clone();` — the local tag applied is the *floating* tag, not `full_ref` (which pins the digest). Even when the digest happens to match, the worker never tags with the digest-ref name; the pin alias the proto comment (`rc.proto:593-594`) describes ("so existing digest pins keep resolving") is never actually applied. Combined with the missing digest verification, this is the core of the blocker above.

---

## 2. Security

### [major] Push/pull accept admin-supplied arbitrary `worker_id`/`worker_ids` with no authorization scoping, and the pull target list is client-controlled
`admin.rs:886-933`: `push_image`/`pull_image` take `worker_id`/`worker_ids` from the request body. The `AdminUser` gate (`admin.rs:90-105`) is the only control — fine for admin-only by design. But `dispatch_pull` (`images.rs:469-478`) also defaults to ALL online workers when the list is empty; combined with the missing digest verification (§1), an admin can inadvertently pull a wrong image onto the whole fleet. Also `pull_image` accepts an arbitrary list that is not validated against connected workers — a stale/unknown worker id silently errors and is dropped (`send_mirror` fails → not added to `out`), and the caller gets a misleading "pulled onto N workers" count (see §3).

### [major] No validation that the registry host is the configured one
`dispatch_push`/`dispatch_pull` build `remote_ref` from `image_remote_ref()` (config-derived, good) — BUT the `ImageMirrorOrder` carries an unvalidated `remote_ref` string to the worker, and `runner.mirror_image_inner` (`runner.rs:601-661`) pushes/pulls **any** remote the control plane sends. An admin (or a bug, or a future caller) can point workers at an arbitrary registry host and the worker will use its local docker credentials for that host. Combined with the `registry_credentials_for` prefix matching (§1), this can leak credentials to a lookalike host. At minimum the server should restrict `remote_ref` to the configured registry, and ideally the worker should refuse refs whose registry host is not the one it was told to use.

### [minor] Credentials are read from filesystem on every call; no masking, no logging review
`docker.rs:751-806` reads `~/.docker/config.json`, `/var/lib/rc-worker/.docker/config.json`, and `/root/.docker/config.json` on every push/pull. `serveraddress` and password are held in memory for the duration of the bollard call — not persisted (good). But: the worker reads *all* auths and picks by prefix; if a push target host isn't in the config, `registry_credentials_for` returns `None` and bollard pushes unauthenticated (usually a 401). No retry guidance or message like "did you docker login on the worker?" surfaces in the UI (`ImageMirrorDone.message` is the raw error). Minor: an operator can't tell whether the failure is credentials vs. network.

### [minor] `raise_alert` rule dedup means repeated mirror failures after a successful one raise no alert
`images.rs:565-571`: alert rule is `image_mirror:{op}:{env_id}`; `store.raise_alert` (`store.rs:1937-1952`) only inserts when no **open** alert with that rule exists, and nothing ever resolves it. A transient failure alerts once forever (until manually resolved), and repeated failures after a fix-and-break cycle are silent. Same pattern exists for image build alerts, so this is consistent, but for a mirror feature that is explicitly admin-driven it's a UX gap — the alert stays "open" even after a successful push.

### [nit] No audit of which worker pushed/pulled
`admin.rs:900-903, 924-931` audit only `push_image`/`pull_image` with the remote ref / worker count — the worker id and op outcome (success/failure) are not recorded in the audit log, so there's no way to answer "did the fleet actually get the image" from the audit trail.

---

## 3. Race conditions / multi-worker pull status

### [major] Single shared `MirrorStatus` row per env means multi-worker pull state is lossy and ambiguous
`images.rs:381-409`: `mirror_status`/`save_mirror_status` store ONE JSON blob under `image_mirror:{env_id}`. `dispatch_pull` (`images.rs:483-504`) overwrites this row once per worker ("Last write wins for the shared status"), then `on_mirror_done` overwrites it per completion. Consequences:
- The UI (`Images.tsx` `MirrorBadge`, `review.mirror`) shows only the *last* worker's status. With N workers, the page can show "pulled" (from the last finisher) while some workers failed, or "error" while most succeeded.
- If a pull for worker A is in flight when worker B's `MirrorDone` arrives, the in-flight status is clobbered; a later failure of A overwrites B's success. The UI cannot represent "pushed to 3/5 workers".
- There's no timeout/expiry on the "pulling"/"pushing" status: if a worker dies mid-pull (channel drops, process killed), no `MirrorDone` ever arrives and the UI shows "pulling" forever. `dispatch_pull` sends the order, and if `send_mirror` fails the status was already saved as "pulling" — and remains so.

### [minor] `dispatch_pull` reports "pulled onto N workers" without waiting for completion
`admin.rs:906-933` returns `ok: true, mirrors: [statuses]` immediately after enqueueing. The success count in the audit detail (`format!("{} worker(s)", sts.len())`) is the *enqueue* count, not the completion count. Combined with the single status row, the audit trail overstates distribution. An SSE `ImageUpdated` event does arrive on completion (`images.rs:572-577`), so the UI can refresh, but the API response itself is misleading.

### [minor] No per-worker dedup of pull: a double-click or retry re-pulls everywhere
`mirror.mutate` in `Images.tsx` disables the button while pending, but two admins (or a retry) can issue overlapping pulls; `dispatch_pull` has no claim/serialization on (env, worker) — worker-side `docker pull` is idempotent-ish but re-tags and re-pushes can race with an in-flight pull on the same worker, producing "denied" or "tag already in use" errors (docker errors on concurrent `tag` of the same source are rare but possible). Minor.

---

## 4. Missing tests

- **No tests** for `dispatch_push`/`dispatch_pull`/`on_mirror_done` at all (`images.rs` tests cover only build lifecycle). No test that `on_mirror_done` records/verifies a digest, or that pull applies `also_local_tag`.
- `image_remote_ref` has one happy-path test (`config.rs:196-206`); missing: registry disabled, empty host, host with port, prefix with slashes, env_id shorter than 10 chars (the `env_id.len() >= 10` slice guard — for a 16-char env id this is always true, but there is no test).
- `split_repo_tag` has only 2 cases (`docker.rs:837-843`); missing: digest refs (`repo@sha256:…`), host:port, `ubuntu`, no-colon repo, and the `runner.rs` push-tag-split reimplementation has **zero** tests.
- `registry_credentials_for` has no tests: prefix-match confusion, `auth` vs `username`/`password` entries, `DOCKER_CONFIG` override, base64 no-pad.
- `mirror_image_inner` (runner) has no tests: unknown op, empty remote, pull+also_tag, push tag-splitting, local_ref digest fallback.
- Web: `Images.tsx` `MirrorBadge` tone mapping is untested (minor; no web test infra likely exists).
- No integration test that pushes then pulls and asserts the pulled digest matches.

---

## 5. API/UI gaps vs product intent

- **Push has no UI to choose the worker**; `push_image` accepts `worker_id` but the UI always POSTs `{}` (`Images.tsx:44-47`), so the flag `prefer_has_image` and worker selection are dead in practice — the first online worker is used, and the whole push fails if that worker lacks the image. The product intent ("push a built image ... from one worker") needs at least a worker picker or a fallback: pick a worker that has the digest, or send the push to every worker that does.
- **Pull has no UI to choose workers**; it always pulls to ALL online workers. Product intent says "pull it onto this worker" (per `rc.proto:584` comment) — the UI offers no per-worker pull. A one-off pull to a single worker (e.g. a new worker joining mid-flight) is impossible from the UI; the API supports it only via the undocumented `worker_ids` body.
- **Mirror status is only one per env** (see §3): the UI shows a single badge, so "3/5 workers have the image" is unrepresentable, and there's no "pulling"→"pulled" history. The worker column in the table (Images.tsx "Hub") shows only the last status.
- **No digest verification surfaced in UI**: after a pull the UI cannot tell whether the fleet's local digest matches the approved `row.digest` (see §1).
- **No registry connectivity check** in Settings: the "镜像仓库" card only shows a static example (`Images.tsx:79-88`); there is no "test connection" or "does this worker have credentials" probe, so a misconfigured registry is discovered only when a push/pull errors.
- **`ImageMirrorOrder.also_local_tag`** is sent with the *same value* as `local_ref` for pulls (`images.rs:484-490`) — the proto comment says it should be the local digest-pin name; sending the same string twice is at best redundant and at worst actively re-tags the wrong thing (see §1). The two fields are effectively conflated in `dispatch_pull`.
- **Proto comment mismatch**: `rc.proto:592-594` documents `also_local_tag` as "so existing digest pins keep resolving", but nothing implements the digest-pin alias; the comment oversells the behavior.
- **`registry` block in `list_images` response** exposes `enabled/host/prefix` to any authenticated user (`_u: User`, `admin.rs:652-689`) — acceptable (viewers already see worker archs), but the same block is absent from `get_image`, so the detail modal can't show the remote ref unless it was loaded via the list.

---

## 6. Severity summary

| # | Severity | Issue |
|---|----------|-------|
| 1 | **blocker** | Pull doesn't verify/record the pulled digest; `also_local_tag` re-tags local names with unverified bytes; digest pins can point at different content |
| 2 | **major** | Push `prefer_has_image` is dead code; push targets the first online worker regardless of whether it has the image |
| 3 | **major** | No validation that `remote_ref`/registry host is the configured one; worker will use local docker credentials against arbitrary hosts |
| 4 | **major** | Single shared mirror status row makes multi-worker pull state lossy/ambiguous; no timeout on in-flight status |
| 5 | **major** | Push tags a floating tag (not digest-pinned); rebuild+repush silently diverges from what workers pulled earlier |
| 6 | minor | `pull_and_tag`/`also_local_tag` can re-tag an unrelated local name (`ubuntu:24.04`) for upstream-image envs |
| 7 | minor | Credential lookup uses prefix/substring host match; can pick the wrong auth entry |
| 8 | minor | No audit of which worker pushed/pulled or of outcomes |
| 9 | minor | Alerts for mirror failures are never auto-resolved; repeated failures after a success are silent |
| 10 | minor | API reports enqueue counts, not completion; no per-worker dedup; no timeout/expiry of in-flight status |
| 11 | minor | UI: no worker picker for push or pull; no digest-vs-local verification surfaced; no registry connectivity check |
| 12 | nit | Proto comment oversells `also_local_tag`; `dispatch_pull` conflates `also_local_tag` and `local_ref` |
| 13 | nit | Missing tests throughout (mirror dispatch, split_repo_tag edge cases, credentials, digest verification) |
