# remote-compile

A remote compile-check service built for coding agents.

Many agents working in parallel worktrees means many `target/` directories —
each several GB — and a lot of CPU. Worse, an agent that runs `cargo check`
itself pulls tens of thousands of log lines into its context window.

remote-compile moves the build onto a pool of compile machines and hands the
agent back a verdict:

```
✗ 2 errors, 1 warnings [compile_error]
task_id=t-01K9…  synced=53B  build=8420ms
代码问题：按结构化诊断修改源码。

E src/main.rs:7:9   E0308  mismatched types
E src/lib.rs:22:5   E0433  failed to resolve: use of undeclared crate `foo`

需要细节: get_log(task_id="t-01K9…", grep="error", limit=50)
```

The agent's whole input is one path. Scanning, hashing, uploading, scheduling
and polling happen in code, where they cost no tokens.

Full design rationale: [`docs/DESIGN.md`](docs/DESIGN.md). Section references
throughout the source point back at it.

## Architecture

| crate | runs on | responsibility |
|---|---|---|
| `rc-agent` | each dev machine | MCP server (stdio); workspace scan, CAS upload, polling |
| `rc-server` | one control-plane host | gRPC API, task queue, scheduler, CAS, SQLite, admin console |
| `rc-worker` | Linux compile machines | Docker sandbox execution, caches, egress proxy |
| `rc-core` | shared | protocol, fingerprint, manifest, build profiles, diagnostics |

```
coding agent ──MCP/stdio──► rc-agent ──gRPC──► rc-server ──gRPC──► rc-worker ×N
                                                   │                     │
                                            SQLite + CAS          Docker sandbox
                                            admin console         sccache / volumes
```

## Quick start

```bash
# 1. build everything (the console is embedded into the rc-server binary)
cd web && npm install && npm run build && cd ..
cargo build --release

# 2. control plane
./target/release/rc-server --data-dir ./rc-data admin --username admin --password <password>
./target/release/rc-server --data-dir ./rc-data serve
#   console → http://127.0.0.1:7700     agents/workers → 127.0.0.1:7701

# 3. a compile machine (Linux + Docker)
./target/release/rc-server --data-dir ./rc-data enroll-token          # prints a single-use token
sudo RC_SERVER=http://<control-plane>:7701 RC_ENROLLMENT_TOKEN=<token> \
     deploy/worker-install.sh

# 4. a dev machine
./target/release/rc-server --data-dir ./rc-data agent-token           # prints an agent token
rc-agent configure --server http://<control-plane>:7701 --token <token>
```

Register the MCP server with your coding agent:

```json
{
  "mcpServers": {
    "remote-compile": { "command": "rc-agent", "args": ["serve"] }
  }
}
```

Then verify from a terminal:

```bash
rc-agent check /path/to/your/repo
```

### First run: environments need approval

A brand-new deployment has no approved build environment, and `check` will say
so and point at `prepare_env`. This is deliberate — a Dockerfile executes
arbitrary commands *at build time*, which the runtime sandbox cannot contain
(§8.3). Either let an agent submit one and approve it in **Images → 审批队列**,
or build the reference image yourself:

```bash
docker build -f deploy/Dockerfile.rust-env -t rc-registry:5000/env/rust:1 .
docker push rc-registry:5000/env/rust:1
# then approve the digest in the console, or:
rc-server --data-dir ./rc-data approve-image <env_id>
```

## MCP tools

| tool | what it is for |
|---|---|
| `check(path, task?, command?, wait_secs?, no_cache?)` | the main one — verdict plus structured diagnostics |
| `get_result(task_id, wait_secs?)` | poll an async task; costs almost nothing |
| `get_log(task_id, offset?, limit?, grep?, tail?)` | paged full log — paging is mandatory |
| `get_build_profile(path)` | what the fleet already knows about building this project |
| `list_envs(query?, arch?, target?)` | find an existing environment before building one |
| `prepare_env(dockerfile\|image, reason?)` | request an environment; always async |
| `get_env_status(env_id)` | build progress and health |
| `list_workers()` | resource-pool overview, for diagnosing a stuck queue |

`check` returns one of five result kinds, because the agent's next move differs
completely between them (§3.5):

| kind | meaning | what the agent should do |
|---|---|---|
| `success` | compiled | carry on |
| `compile_error` | code problem | fix the source, using the diagnostics |
| `env_error` | the image is missing something | find or build an environment |
| `infra_error` | worker/disk/daemon failure | nothing — already retried on other machines |
| `timeout` | killed by the hard limit | split the task or raise `timeout_secs` |

## Per-repo configuration

Drop `.remote-compile.toml` at the repo root to pin how it builds. It is
versioned, reviewable, travels with the branch, and outranks anything the
control plane has learned (§3.2):

```toml
adapter = "rust"
image = "rc-registry/env/rust-protoc@sha256:a3f9…"   # must be a digest
path = "crates/backend"                              # monorepo sub-project
env = { RUSTFLAGS = "-C target-cpu=native" }
features = ["ssr"]
pre_commands = ["cargo run -p xtask codegen"]

[tasks]
check  = "cargo check --workspace --all-targets"
test   = "cargo nextest run -p backend"
clippy = "cargo clippy -- -D warnings"
```

Resolution order: explicit call arguments → this file → what the control plane
stored → adapter auto-detection. After a project's first green build the agent
publishes what worked, so the next agent inherits it.

## Admin console

Served by the `rc-server` binary itself at `/` — there is no separate frontend
deployment.

- **大盘** — throughput, success rate, cache hit rate, phase percentiles, live worker pool
- **任务** — filter by project/worktree/session/status/result; detail page has the phase waterfall, structured diagnostics, retry history and a virtualised log viewer with grep and tail
- **Worker** — load, disk, cache inventory; drain / resume / remove; enrollment tokens
- **镜像** — the approval queue (read the Dockerfile before approving) and health trends
- **构建档案** — what the fleet has learned per project
- **存储** — CAS accounting, GC policy, manual GC
- **设置** — scheduling weights, TTLs, agent tokens, accounts, audit log

Live state changes arrive over SSE; charts poll. Two roles: `admin` (everything)
and `viewer` (read-only).

## Monitoring

Built-in time series with zero dependencies: samples accumulate in memory and
flush to SQLite as batched rollups (1-minute buckets kept 7 days, hourly kept
90 days). Worker heartbeats stay in memory and never hit SQLite at heartbeat
frequency — a single-writer database cannot absorb that (§15.1).

`GET /metrics` exposes the same counters in Prometheus format for deployments
that already have a monitoring stack. It is unauthenticated so scrapers can
reach it; keep the admin port off the public internet.

Built-in alert rules (worker offline, queue backlog, infra-error rate, timeout
rate, failing image) fire once on transition to a webhook that DingTalk, Feishu
and Slack all accept.

## Security model

- Agent-submitted code is untrusted: `build.rs` and proc-macros run arbitrary
  code during a plain `cargo check`. Containers drop all capabilities, get a
  read-only root, memory/CPU/pid caps and a hard timeout (§7.1).
- Build containers sit on an `internal` Docker network whose only reachable
  address is the host gateway, where an allowlist proxy runs. Cleartext HTTP is
  restricted to GET/HEAD plus git's `git-upload-pack` POST; HTTPS CONNECT is
  host-allowlisted with a per-tunnel byte cap. **Known residual risk:** anything
  that can issue GETs to crates.io or github can encode data outbound. This
  narrows the pipe; it does not close it (§16).
- New image digests require admin approval before they may execute code (§8.3).
- Three separate principals: agent bearer tokens, worker tokens issued through
  single-use enrollment, and console sessions. All are stored hashed.
- Holding the Docker socket is equivalent to root, so **compile machines must be
  dedicated** and not share hardware with other workloads.
- Code blobs sit in the CAS unencrypted at rest; evaluate that against your own
  requirements before storing sensitive source (§16).
- TLS is a deployment concern: run both ports behind a reverse proxy
  (`deploy/caddy.example`). Agents and workers pick their transport from the
  URL they are configured with — an `https://` endpoint gets TLS against the
  system trust store, `http://` stays cleartext and belongs on loopback or a
  private network.

## Development

```bash
cargo test --workspace        # 263 unit tests
cargo clippy --workspace --all-targets
./scripts/smoke.sh            # 36 end-to-end checks against real binaries

cd web && npm run dev         # console with hot reload, proxying to :7700
```

`scripts/smoke.sh` starts a real control plane, drives the MCP server over
stdio, and asserts the whole admission path: the approval gate, L1 baseline
sync, L2 content-addressed sync, CAS dedup, blob pinning and supersede.

## Status

Implements the v0.3 design. Not yet built:

- **Language adapters** beyond Rust — the `Adapter` trait and a generic
  text-scraping fallback exist; C/C++ and Go detection do not (§10.3).
- **Windows workers** — Linux only, as designed (§1.2).
- **Postgres** — SQLite only; the storage layer is a single module to swap.
- **TLS in-process** — reverse proxy instead, see above.
- **gVisor / firecracker** isolation (§7.1).
