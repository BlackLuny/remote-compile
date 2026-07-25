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

### Missing system libraries

Native dependencies are the common way a build fails on a machine that is not
yours: crates ending in `-sys` wrap a C library that has to be installed
separately, and a mid-size Rust project drags in dozens of them. The failure
lands as `env_error`, which is the important part — the agent is not sent to
edit source that was never wrong. But "the environment is missing something"
is not actionable on its own, and the one line that names the library sits at
no predictable place in a log thousands of lines long. Grepping for it means
already knowing the name you are trying to learn.

So `check` lifts it out and reports it inline:

```
✗ 环境错误（exit 101）：error: failed to run custom build command for `rrd-sys v0.1.3` [env_error]
环境缺依赖：用 list_envs 找可用镜像，或 prepare_env 提交 Dockerfile。
构建日志显示环境缺少以下依赖:
  - pkg-config 模块 `librrd` 未找到 → 可能是 librrd-dev（按命名惯例推测，需核实）
  安装建议: apt-get install -y librrd-dev
```

Recognised: failed pkg-config probes, `cannot find -lfoo`, missing headers,
CMake's `Could NOT find X`, and executables that were not on `PATH`.

Two rules keep it honest, because a wrong answer costs more than no answer —
acting on one means asking a human to approve a Docker image that fixes nothing:

- **The name is read from the log; the package is inferred.** A guessed package
  says so. The convention `<x> → lib<x>-dev` is right for `librrd` and `zstd`
  and wrong for `openssl` (`libssl-dev`) and `alsa` (`libasound2-dev`), so known
  mappings come first and are kept separate for libraries and for programs —
  `curl` the command is `curl`, `curl` the library is `libcurl4-openssl-dev`.
  An unrecognised *executable* gets no guess at all, because nothing links a
  binary name to the package that ships it. Neither does a failing `-sys` crate:
  those fail over vendored sources and configuration at least as often as over a
  missing library.
- **A guess needs a shape, not just an absence.** "Could not find X" is ordinary
  English that appears throughout cargo's own output, so X is believed only when
  it is a name already known or is visibly a library.

A missing header is also *reclassified*: `fatal error: openssl/ssl.h: No such
file` parses as a compiler error, but it is not one an agent can fix by editing
code, so it is reported as `env_error` — unless real compile errors appear
alongside it, in which case the code problem wins.

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
extra_roots = ["../private_tun"]                     # see below
exclude = ["*.pem", "secrets/**"]                    # see below

[tasks]
check  = "cargo check --workspace --all-targets"
test   = "cargo nextest run -p backend"
clippy = "cargo clippy -- -D warnings"
```

Resolution order: explicit call arguments → this file → what the control plane
stored → adapter auto-detection. After a project's first green build the agent
publishes what worked, so the next agent inherits it.

### Dependencies outside the repository

A cargo `path` dependency can point at a sibling checkout, and `../private_tun`
has to keep meaning the same thing on the worker. The agent finds those
directories, mounts every root under a common ancestor so their relative
positions survive, and syncs them alongside the repository.

Doing that sends code the caller never named to a CAS that is **not encrypted at
rest**, so it is not done silently. The first time such a directory appears the
check stops and prints the line to add:

```toml
extra_roots = ["../private_tun", "../shadow-tls-tokio"]
extra_roots = "auto"    # allow whatever is discovered
extra_roots = []        # sync nothing outside the repo; the build fails plainly
```

Only the repository's own file grants this — never a profile learned from the
fleet. Directories the repository `.gitignore`s but cargo still builds need
listing too, for the same reason.

Discovery covers path dependencies (including ones inherited from
`[workspace.dependencies]` or declared per-target), workspace members outside the
repository, and `[patch]`/`[replace]` entries in the workspace manifest and
`.cargo/config.toml`. When it cannot promise a complete answer it refuses to
build rather than compiling against a guess.

### Keeping files off the wire

§4.3 makes git the source of truth for enumeration, so everything git tracks is
synced. That is the right default — an ignored-but-load-bearing file breaking
only on the worker is the bug the rule exists to prevent — but it leaves no way
to withhold a credential the repository already tracks. `.gitignore` cannot help
with a file that is already committed.

```toml
exclude = ["*.pem", "secrets/**", "fixtures/customer-data.json"]
```

gitignore syntax, including `!` to re-admit. Patterns are matched against paths
relative to each root.

Setting `exclude` at all turns the L1 git baseline off for that repository, and
says so. It has to: the baseline ships as a `git bundle`, and a bundle carries
**reachable history**, not just the tree at that commit. A key staged for
deletion is already gone from `git ls-files` while still in `HEAD`; one deleted
three commits ago is gone from both and still in the pack. No check over the
current tree can vouch for an object graph, so none is attempted. The remaining
files travel individually, which is slower until the CAS warms up — that is the
price of the exclusion meaning something.

A directory name excludes everything beneath it, as in git, and a negation
cannot reopen an excluded directory. Patterns apply to **every** synced root,
not just the primary: a key is a key wherever it sits. That errs towards
withholding, and withholding too much breaks the build loudly rather than
leaking quietly.

Two things it does not do. It does not retroactively remove anything already
uploaded; a key synced before the exclusion needs the server's CAS and the
worker's git mirror cleaned out by hand. And it does not make the build work
without the file — if the build reads it, the build fails remotely and passes
locally, which is exactly the divergence §4.3 warns about. Every result names
the active patterns for that reason.

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
cargo test --workspace        # 413 unit tests
cargo clippy --workspace --all-targets
./scripts/smoke.sh            # 52 end-to-end checks against real binaries

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
- **Shared compilation cache** — off, and not merely unconfigured: §7.2 puts the
  sccache *server* on the worker host so that cache credentials stay out of
  untrusted containers, but the server is the half that invokes the compiler,
  and the toolchain path it is handed exists only inside the build image. Every
  compile fails. Fixing it means running sccache inside the container with
  `SCCACHE_DIR` on a mounted volume — which gives up that credential isolation
  and lets build code poison a cache shared across projects, so it is left as a
  decision rather than a patch. `RC_ENABLE_SCCACHE=1` forces the old path back
  on, but the reference image no longer carries the client binary, so that also
  means rebuilding an environment. Local crates are still cached by the
  per-worktree target volume, which is where most of the benefit was.
