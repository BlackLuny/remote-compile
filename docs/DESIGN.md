# remote-compile 设计文档

> 状态：草案 v0.3（评审修订版）
> 最后更新：2026-07-24

## 1. 背景与目标

多 agent 并行编码的场景下，每个 agent 在独立 worktree 中工作。Rust 等编译型语言的编译产物（`target/` 动辄数 GB）在本地堆积，磁盘和 CPU 成为瓶颈；agent 自行运行 `cargo check` 还会把海量编译日志吞进上下文，浪费 token。

remote-compile 是一个**面向编码 agent 的远程编译检查服务**：

- 把 check/build 卸载到编译机资源池，本地零产物；
- 对 agent 透明：默认只返回结论和结构化错误摘要，token 成本极低；
- 增量优先：代码同步、编译缓存、任务去重全部按"避免全量"设计。

### 1.1 设计原则

1. **Token 经济优先**：agent 只接触极简输入（一个路径）和分级输出（结论 → 摘要 → 分页日志）。所有扫描、哈希、传输、轮询由本地 MCP server 用纯代码完成。
2. **内容寻址**：代码、缓存、任务去重都以内容 hash 为依据，不依赖 git 提交状态（agent 的改动绝大多数未提交）。
3. **不信任输入**：agent 提交的代码视为不可信（build.rs / proc-macro 可执行任意代码），执行环境默认沙箱化。
4. **Fleet 学习**：构建环境、Build Profile 等知识全 fleet 共享，一个 agent 趟过的坑其他 agent 不再趟。
5. **先简单后扩展**：v0 单控制面 + 裸 Docker，不上 K8s；单机能跑通的东西不分布式化。

### 1.2 非目标（v0）

- Windows worker（仅 Linux，后续支持）；
- macOS / Windows 目标的检查语义：worker 仅 Linux，`cfg(target_os)` 分支代码的远端 check 结果与本地 mac/win 构建可能不同；apple/msvc target 交叉编译需专有 SDK，v0 不做。文档明示，避免使用方按错误预期接入；
- 高可用控制面（单实例 + SQLite）；
- 多租户与计费；
- 细粒度分布式编译（单条 cargo 命令拆到多台机器）——不做，靠 sccache 已足够。

## 2. 总体架构

```
┌─ 开发机（每台）────────────────────────────────────┐
│  编码 Agent                                        │
│    │ MCP (stdio)                                   │
│    ▼                                               │
│  rc-agent (本地 MCP server，常驻进程)                │
│   ├─ 工作区扫描 / manifest / blake3                 │
│   ├─ stat 索引缓存 (SQLite)                         │
│   ├─ CAS 上传（增量）                                │
│   ├─ 任务提交 / 轮询                                 │
│   └─ 本地结果缓存（内容指纹 → 上次结果）              │
└───────┼────────────────────────────────────────────┘
        │ HTTPS / gRPC（双向 token 认证）
        ▼
┌─ rc-server (控制面，单实例) ───────────────────────┐
│  ├─ API: agent 接口 / worker 接口 / 管理接口         │
│  ├─ 任务队列：supersede、去重、重试、超时             │
│  ├─ 调度器：磁盘 / CPU / 缓存亲和 / 镜像亲和打分      │
│  ├─ 环境注册中心：worker、镜像元数据、健康度          │
│  ├─ Build Profile 存储                              │
│  ├─ CAS（内容寻址存储）+ 全量日志存储（zstd）         │
│  ├─ Admin REST API + SSE（供管理后台 React SPA，内嵌托管）│
│  ├─ 管理后台（web/：监控大盘、镜像审批、worker/任务管理）  │
│  ├─ 内置时序监控（rollup 存 SQLite）+ /metrics 导出        │
│  └─ SQLite（v0）                                    │
└───────┼────────────────────────────────────────────┘
        │ gRPC（长连接，worker 主动连入）
        ▼
┌─ rc-worker × N（Linux 编译机，一键安装）───────────┐
│  ├─ git mirror（基线层增量同步）                     │
│  ├─ 工作区重建（CAS → 容器内目录）                   │
│  ├─ Docker 沙箱执行（断网 + 限额 + 超时）            │
│  ├─ 白名单出口代理（crates.io / github 等）          │
│  ├─ sccache（共享 Redis/S3 后端）                   │
│  ├─ cargo registry 缓存 volume（每项目共享）         │
│  ├─ target volume（按 project+worktree 持久化）      │
│  └─ BuildKit（agent 提交的 Dockerfile 构建镜像）     │
└────────────────────────────────────────────────────┘
```

四个进程/服务，全部 Rust 实现，同一 workspace：

| crate | 部署位置 | 职责 |
|---|---|---|
| `rc-agent` | 开发机 | MCP server，本地扫描/上传/轮询 |
| `rc-server` | 控制面服务器 | API、调度、CAS、状态存储、管理后台 |
| `rc-worker` | 编译机 | 任务执行、Docker 管理、缓存管理 |
| `rc-core` | 共享库 | 协议定义（prost/tonic）、数据模型、指纹算法、诊断解析 |

## 3. 核心实体与数据模型

### 3.1 Project / Worktree

- **Project**：一个代码库，以远程 URL 或本地根路径 canonical 化后标识。非 git 目录也允许（见 §5.4 降级）。
- **Worktree**：Project 下的一个工作区。agent 场景下多 worktree 并行是常态。worktree_id 由 rc-agent 本地生成并上报（= worktree 路径 hash + 首次见到的 base commit），服务端不假设它与 git worktree 一一对应。

### 3.2 BuildProfile（构建档案）

解决"不同项目构建差异"的核心实体。每个 (project, 子路径) 一份：

```toml
# 仓库内可放置 .remote-compile.toml，结构一致，优先级高于服务端存储
adapter = "rust"                      # 诊断解析器
image = "rc-registry/env/rust-protoc:a3f9"
path = "crates/backend"               # monorepo 子项目，可选
env = { RUSTFLAGS = "-C target-cpu=native" }
features = ["ssr"]
target = "x86_64-unknown-linux-musl"  # 跨平台，可选
pre_commands = ["cargo run -p xtask codegen"]

[tasks]
check  = "cargo check --workspace --all-targets"
test   = "cargo nextest run -p backend"
clippy = "cargo clippy -- -D warnings"
```

**解析优先级链**（高 → 低）：

1. 调用方显式传参；
2. 仓库内 `.remote-compile.toml`（随 git 走，版本化、人可审查）；
3. 服务端已存 profile（其他 agent 摸索出的，fleet 共享）；
4. 适配器自动探测生成的初始 profile（读 `Cargo.toml` / `rust-toolchain.toml` / `.cargo/config.toml` 等）。

profile 附带健康元数据：最近成功时间、成功率趋势、创建者（agent/admin/auto）、关联镜像。

**写入策略**：agent 新建/修改 profile 若指向**未信任镜像**，镜像需管理员审批通过后才生效（见 §8.3）；已信任镜像之间的切换直接生效。

### 3.3 EnvImage（环境镜像）

```yaml
image_id: rc-registry/env/rust-protoc:a3f9
source:
  dockerfile: <内容>          # 或上游镜像 ref
  context_repo: github.com/org/repo
  commit: a1b2c3
arch: [x86_64, aarch64]
targets: [x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc]  # 支持的编译目标
status: pending_approval | building | healthy | failing | rejected
health:
  last_success_at: 2026-07-23T10:00:00Z
  success_rate_7d: 0.97
  total_runs: 132
used_by: [project-a, project-b]
built_at: ...
builder_worker: worker-3
```

健康度信息通过 MCP 暴露给 agent，用于"是否已有可用环境"的判断。

### 3.4 Worker

```yaml
worker_id: worker-3
labels: { arch: x86_64, gpu: "none" }
capacity: { cpu: 32, mem_gb: 128, disk_gb: 2000 }
status: online | draining | offline
stats:                        # 心跳上报，参与调度
  cpu_load: 0.42
  disk_free_gb: 830
  running_tasks: 3
  cached_projects: [proj-a]   # 本地有 target volume / git mirror 的项目
  cached_images: [...]
version: 0.3.1
enrolled_at: ...
last_heartbeat_at: ...
```

### 3.5 Task

```yaml
task_id: t-01J...
type: check | build | test | clippy | custom
project / worktree / agent_session
profile_ref: ...
fingerprint: blake3(...)      # 见 §6.2
supersede_key: (worktree, agent_session, type)
status: pending | syncing | queued | running | uploading | done | failed | canceled | superseded
result:
  kind: success | compile_error | env_error | infra_error | timeout
  diagnostics: [...]          # 结构化摘要
  log_ref: cas://...          # 全量日志
  stats: { queue_ms, sync_ms, build_ms, cache_hit_rate }
attempts: [{worker, started, ended, error}]
```

**结果类型必须严格区分**，agent 的后续动作完全不同：

| kind | 含义 | agent 应做的 |
|---|---|---|
| `success` | 编译通过 | 继续 |
| `compile_error` | 代码问题 | 修代码（带结构化诊断） |
| `env_error` | 镜像/环境缺东西 | 查环境、修 Dockerfile、换镜像 |
| `infra_error` | worker 故障/磁盘满/拉镜像失败 | 无需动作，系统自动换机重试，重试耗尽才上报 |
| `timeout` | 超时被杀 | 视情况拆分任务或调大超时 |

**env_error 必须说清缺什么。** 分类正确只解决了一半问题：agent 知道不该改代码，
却不知道该往镜像里加什么。原生依赖缺失是最常见的一类（`-sys` crate 包裹 C 库，
中等规模项目动辄几十个），而点名那个库的一行往往埋在几千行日志的末尾——让 agent
翻日志正是本系统要消除的 context 开销（§11）。因此在分类时就从日志里把证据提取
出来随结果返回：pkg-config 探测失败、`cannot find -lfoo`、缺头文件、可执行文件
不在 PATH。

**库名是事实，包名是猜测，二者必须可区分。** `<x> → lib<x>-dev` 这个惯例对
`librrd`、`zstd` 成立，对 `openssl`（`libssl-dev`）、`alsa`（`libasound2-dev`）
不成立，所以已知映射表优先，落到惯例的一律标注为推测。映射表按"库/可执行文件"
分开——`curl` 命令来自 `curl` 包，`curl` 库来自 `libcurl4-openssl-dev`，一张表
必然把其中一个搞错还声称确定。不认识的可执行文件不给猜测（二进制名与包名之间
没有命名规律，编一个 `libprotoc-dev` 比不说更糟）；失败的 `-sys` crate 同样不给
——它因 vendored 源码、配置、自身 panic 而失败的概率不低于缺库。

**猜测需要形状，不能只凭"没找到"。** "Could not find X" 是普通英语，cargo 自己
的输出里到处都是。所以只在 X 是已知名字或形如 `lib…` 时才采信——用白名单挡，
不用英文词黑名单挡，后者永远列不全。同一个名字有更强证据时以更强的为准。

**缺头文件要改判。** `fatal error: openssl/ssl.h: No such file` 会被通用适配器
（§10.3）解析成一条格式完好的 error 诊断，报成 `compile_error` 正是 risk #4：
把 agent 支去改没错的代码。因此当所有 error 诊断都是这种形状时改判 env_error；
只要混有真正的编译错误，就仍算代码问题——反向误判会把 agent 真正该修的诊断
藏起来，更危险。

## 4. 代码同步

### 4.1 分层传输

agent 的改动绝大多数**未提交、无 sha**，因此同步不依赖 git 状态，分两层：

**L1 基线层（有 sha 的部分）**：worktree 通常基于某个 commit。worker 上维护 project 的 git mirror，基线层优先增量同步，同 commit 永不重传。

`git fetch` 拿不到 base commit 是**常态而非异常**——私有仓库的凭据只在开发机上，且 agent 频繁本地 commit（未 push，上游根本没有该 sha）。获取降级链：

1. mirror 已有该 commit → 直接用；
2. 上游匿名可达且 commit 已 push → `git fetch`；
3. 以上都不行 → rc-agent 端 `git bundle` 打包缺失的 commit 链（server 告知 mirror 已有的最近祖先作为 bundle 基点），经 CAS 上传，worker 从 bundle 导入 mirror；
4. bundle 也不可行（浅克隆、仓库损坏等）→ 该 worktree 降级为**全 L2 同步**。CAS 内容去重保证只有首次是全量传输。

worker 一律**不持有上游 git 凭据**；mirror 的更新来源只有匿名可达的公开仓库或 agent 上传的 bundle。

**L2 脏改动层（无 sha 的部分）**：内容寻址同步：

1. rc-agent 枚举工作区文件（见 §4.3），生成 manifest：`{path: (size, mode, blake3_hash, type)}`，mode 记录可执行位；
2. 把 hash 列表发给 server 对账（"你缺哪些"；对账即续租，见 §4.7）；
3. 只上传缺失 blob，server 存入 CAS（blake3 即 key）；
4. worker 执行任务前，用 CAS 在构建容器内重建工作区（L1 checkout + L2 overlay）。

收益：

- **跨 worktree / 跨 agent 天然去重**：同一内容全系统只存一份、只传一次。`Cargo.lock`、依赖源码等大文件永不重传；
- **增量极小**：改一个文件只传一个文件；
- CAS 自身是任务级缓存和日志存储的基础。

### 4.2 撕裂防护（torn snapshot）

扫描耗时数秒，期间 agent 可能正在写文件。必须防止"前后不一致的缝合快照"被提交编译（否则 agent 会去修一个不存在的错误）。

- 扫描完成后，对 mtime 发生变化的文件做二次复核；
- 有变化则重扫，最多 N=3 次；
- 仍不稳定 → 返回明确错误 `workspace_unstable, retry later`，绝不提交撕裂快照。

### 4.3 文件枚举：git 为事实源

`.gitignore` 猜测式扫描会漏掉"被 ignore 但构建需要"的文件（本地有远程无，远程报错本地不报，极难排查）。枚举顺序：

1. `git ls-files`（tracked，含已暂存）+ `git status --porcelain`（untracked 但未 ignored）——这正是"本地构建能看到什么"的定义；
2. 非 git 目录降级为 ignore-walk（尊重 `.gitignore` / `.ignore`），行为略有差异但可用。

**submodule 处理**：`git ls-files` 默认只列 gitlink 条目、不枚举子模块内容，而子模块代码是构建的一部分（vendored 依赖、共享 proto 库），漏掉会造成"远程缺文件、本地正常"的分歧。v0 策略：**递归枚举子模块工作区**（`git ls-files --recurse-submodules` + 各子模块内的 untracked 文件），子模块文件一律走 **L2 内容寻址同步**，不为其做 L1 mirror/bundle——子模块改动罕见，CAS 全局去重使重复同步零成本，仅首次遇到新内容全量上传一次。子模块 L1 基线优化留作后续。

始终排除：`target/`、`.git/`、`node_modules/` 等产物目录（adapter 可提供排除清单）。

### 4.4 symlink、大小写、mtime

- **symlink 不 follow**：记录为 `(type=symlink, target=<字符串>)`，哈希 target 字符串而非内容；
- **路径大小写冲突**（macOS 不敏感 / Linux 敏感）：manifest 中检测同目录下仅大小写不同的路径，直接报 `sync_error`，不静默覆盖；
- **mtime 只作快速路径提示，hash 才是判据**：策略向"宁可重传"倾斜（误传无害，漏传是正确性问题）；
- **文件权限只保留可执行位**：manifest 记录 mode 的 +x 位（`pre_commands` 跑脚本依赖它），其余权限位不保证还原。

### 4.5 本地索引存储

rc-agent 的状态统一放在用户级目录，**不污染 worktree**：

```
$XDG_CACHE_HOME/remote-compile/         # 默认 ~/.cache/remote-compile，cache_dir 可覆盖
├── indexes/<blake3(worktree_abs_path)[:16]>.sqlite   # 每 worktree 一个 stat 索引
├── cas_known.sqlite                    # 已确认在 server 端存在的 blob hash
└── results.sqlite                      # 内容指纹 → 上次结果摘要（本地命中直接返回）
```

- 索引格式 SQLite（WAL 模式 + 文件锁，容忍多 agent 实例并发）；表：`files(path, size, mtime, hash)` + `meta(k, v)`；
- worktree 被 mv/重命名 → key miss → 全量重扫重建，代价一次扫描，无正确性问题；
- worktree 已删除 → 启动时校验 path 存在性 + TTL（30 天）清理孤儿索引；
- 卸载 = 删目录即净。

### 4.6 rc-agent 进程形态

v0 采用**单进程 MCP server（stdio）+ 磁盘索引**：被 agent 拉起，CAS/控制面保持长连接，索引落盘使冷启动后仍是增量扫描。

后续若一台开发机上十几个 agent 共享同一代码，再拆 **本地 daemon + 瘦 MCP 前端**（MCP 进程仅转发），对外工具接口不变。

### 4.7 CAS 对账租约与丢失自愈

对账结果与 GC（§9）之间存在竞态——"server 说有"到"worker 实际拉取"之间 blob 可能已被回收。按租约处理：

- **对账即续租**：server 回答"已有"的同时 bump 这些 blob 的 `last_used`，并对本次提交任务引用的全部 blob 加 pin，任务进入终态后解除；GC 绝不回收被 pin 或租约期内的 blob；
- **`cas_known` 只是提示**：rc-agent 本地缓存设 TTL（7 天，远小于服务端 30 天 GC TTL），过期条目重新对账，不作为"无需上传"的最终判据；
- **丢失自愈**：worker 拉取 blob 遇 404 → 回报 `blob_missing`，server 将任务转回 `syncing` 并通知 rc-agent 补传。全程归入 infra 自动重试，不打扰 agent。

## 5. 任务指纹、去重与 supersede

### 5.1 内容指纹

```
fingerprint = blake3(
    manifest_root_hash                   # 全量代码内容（含 mode / symlink 信息）
  + image_digest
  + toolchain                            # 如 rustc 1.85.0
  + blake3(resolved_profile_canonical)   # 解析完成的 profile 全文，规范化后整体哈希
)
```

**不逐字段枚举环境维度**：profile 解析后的最终形态（adapter、command、features、target、env、pre_commands……）规范化后整体哈希。枚举式定义是遗漏的温床——例如 `pre_commands` 会生成代码，漏掉它就会错误命中。整体哈希下 profile 任何字段变动天然失效缓存；**任何遗漏都会把旧结果错发给新场景，属正确性问题**，宁可多编译，不可错命中。

注意：哈希前镜像 tag 必须已解析为 digest（tag 可变、digest 才是内容）——解析发生在 profile resolution 阶段，resolved profile 中的 image 字段只能是 digest。

用途：

- **任务级缓存**：指纹相同 → 直接返回上次结果，连编译都不用跑（rc-agent 本地先查，server 再查）；
- **已知局限**：非确定性构建（build.rs 读时间/随机数）可能返回过期"成功"。任务级缓存设 TTL（默认 24h）并接受该风险，文档明示。

### 5.2 Supersede

同一 `(worktree, agent_session, task_type)` 来新任务时，未开始运行的旧任务取消（agent 只关心"当前代码编不编得过"，旧代码结果无意义）。

- **作用域含 task_type**：agent 对同一份代码连发 check + clippy + test 是常规操作，跨类型不得互相取消；只有"同类任务的旧代码版本"才被 supersede；
- **作用域限定为 agent_session**：两个 agent 在同一 worktree 干活时，A 的新任务不得取消 B 的任务——跨 session 只按指纹去重，不 supersede；
- **agent_session 是稳定标识**：由 rc-agent 首次启动时生成并持久化在本地配置中，不随 MCP 连接重建而变化——重连的同一 agent 不会 supersede 掉自己还在排队的任务；
- **与指纹订阅的交互**：待取消任务若有其他 session 的订阅者（§5.3），不得取消——从本 session 的 supersede 链上摘除，降级为"仅为订阅者执行"；无订阅者才真正取消。否则订阅者会永远 pending。

### 5.3 队列行为

- 指纹相同的 pending/running 任务已存在 → 直接挂到该任务上等结果（订阅而非新建）；被订阅任务遭 supersede 的处理见 §5.2；
- **遗弃清理**：agent 断连**不取消**任务——MCP 会话可能先于任务结束（对话中断/客户端重启），agent 重连后凭 `task_id` 仍应能拿到结果（§12 异步轮询的承诺）。仅靠 pending TTL（默认 30 min）清理长期无人领取的排队任务；
- server 重启：`running` → `pending` 重排队；worker 上的孤儿任务由 worker 心跳对账清理。

## 6. 调度器

### 6.1 打分模型

任务分配到 worker 采用加权打分（所有指标来自心跳上报 + 本地状态）：

```
score = w1 * disk_fit          # 磁盘余量是否 > 预估需求 * 1.5（预估来自该项目历史统计，新项目用默认值）
      + w2 * (1 - cpu_load)    # CPU 余量
      + w3 * cache_affinity    # 本地已有该 worktree 的 target volume = 1；有 project 的 = 0.6；无 = 0
      + w4 * image_affinity    # 本地已拉取所需镜像 = 1，否则 0
      + w5 * arch_match        # 架构/标签匹配为硬过滤，不参与打分
```

硬过滤先行：架构、磁盘低于阈值（< 10% 或 < 预估需求）、status != online 的直接排除。

### 6.2 约束

- **同一 (worktree, worker) 的任务串行**：共享同一 target volume，cargo 文件锁下并行只会互等。worker 内按 worktree 排队，不同 worktree 之间并行，并行度 = worker 配置；
- **同一 worktree 可用多台 worker**：不同任务（check/clippy/test、不同 target）天然可分布到多机；单条 cargo 命令不拆分；
- **infra_error 自动换机重试**：最多 2 次，且不得落到同一 worker；重试耗尽才上报 agent；
- **磁盘水位**：worker 上报 disk_free；低于阈值触发本地 GC（见 §9），仍低于硬阈值则从调度池摘除并告警。

## 7. 执行环境（rc-worker）

### 7.1 沙箱

agent 提交的代码不可信（build.rs / proc-macro 执行任意代码）。构建容器：

- `--network=none` + 唯一例外：worker 上的**白名单出口代理**（只允许 crates.io / github.com 等源站，由 worker 配置）；registry 缓存命中后多数构建无需网络；
- **代理限制为只读语义**：仅放行 GET/HEAD（git smart HTTP 的 `git-upload-pack` POST 单独放行）。整域放行 github.com 的可写方法，等于给恶意 build.rs 一条源码外泄通道（push 到攻击者仓库、crates.io publish 同理）。即便只读，GET 通道仍可编码外带少量数据，属已知残余风险（§16 明示）；
- `--cap-drop=ALL`、`--read-only` rootfs、tmpfs `/tmp`；
- `--memory` / `--cpus` / `--pids-limit`（防 fork bomb）；
- 单任务硬超时（profile 可配，默认 10 min），超时 `timeout` 结果；
- 容器与 volume 统一打 label `rc.*`，便于 GC 与崩溃后 reconcile。

**Worker 持有 docker socket 等于 root**：编译机必须专用，不与别的业务混部。更强隔离（gVisor / firecracker）列为后续方向。

### 7.2 缓存体系（三层）

| 层 | 机制 | 共享范围 |
|---|---|---|
| 依赖编译缓存 | sccache（server 驻 worker host，见下），后端 Redis/S3 | 跨 worktree、跨 worker、跨 agent |
| target dir | 每 (project, worktree) 一个 docker volume，跟随调度亲和 | 同 worktree 同 worker |
| cargo registry | 每 project 一个共享 volume | 跨 worktree 同 worker |

**注意 sccache 与 cargo incremental 互斥**（sccache 禁用增量）。check 场景组合：依赖 crate 走 sccache（编译量大、跨环境命中），本地 crate 靠 target volume 持久化，不追求 CARGO_INCREMENTAL。

**sccache 连通性**：构建容器 `--network=none`，容器内直连 Redis/S3 不可行（Redis 协议也过不了 HTTP 代理）。方案：sccache server 常驻 worker host，容器内只跑 client，经挂载的 unix socket（`SCCACHE_SERVER_UDS`）通信；远端缓存后端由 host 侧访问，**后端凭据不进入不可信容器**。

> **⚠ 上述方案行不通，共享 sccache 当前关闭。** 调用编译器的是 sccache
> server，而它拿到的编译器路径（`/usr/local/rustup/toolchains/…/bin/rustc`）
> 只存在于容器镜像里——host 上没有这套 toolchain，每个编译请求都在
> server 侧失败并断开连接。凭据隔离这个出发点是对的，但 client/server 的职责
> 边界与之矛盾：要共享缓存，就得让 server 和编译器待在一起。
>
> 可行的修法是把 sccache 整个放进容器，`SCCACHE_DIR` 指向挂载卷——代价是放弃
> 本节想要的凭据隔离，且不可信构建代码能写入跨项目共享的缓存（投毒面）。这个
> 取舍尚未拍板，因此 rc-worker 默认不启用（`RC_ENABLE_SCCACHE=1` 可强开），
> 本地 crate 仍由 per-worktree target volume 缓存。

**rust-toolchain.toml 漂移**：指纹含 toolchain；镜像内置 rustup + 常用 toolchain，缺失时在容器内经白名单代理安装（rustup.rs 加入白名单）。

### 7.3 工作区重建

worker 收到任务 → 从 server CAS 拉取缺失 blob → 容器内重建：L1 `git archive <base_commit>` 展开 + L2 overlay 写入 + symlink / 可执行位恢复。

**manifest 是重建的唯一事实源**：基线中存在但 manifest 中没有的文件必须删除——agent 删文件与改文件同等常见，漏删会造成"远程能编过本地编不过"（或反之），正是 §4.3 极力避免的那类分歧。重建完成后校验目录与 manifest 一致。

重建结果可短期缓存，key 为 `(base_commit, manifest_root_hash)` 而非任务指纹——同一份代码连发 check/clippy/test 指纹不同但工作区相同，按指纹缓存会无谓重建。

## 8. 环境池与镜像管理

### 8.1 Worker 生命周期

- **一键安装**：`curl ... | sh` 或单静态二进制 + `rc-worker install --server ... --token ...`；systemd unit 自动注册；
- **注册认证**：enrollment token（管理后台生成，限时单次使用）→ 换取 worker 证书，之后心跳/任务走双向认证；
- **升级/下线**：`drain` 模式——不接新任务、跑完存量再退出；
- **卸载**：`rc-worker uninstall` 清理自身创建的容器/volume（按 `rc.*` label）、systemd unit、二进制；
- **崩溃恢复**：启动时按 label reconcile，清理僵尸容器；向 server 对账任务状态。

### 8.2 镜像构建

agent 提交 Dockerfile（或引用上游镜像）→ server 记录 `pending_approval`（见 §8.3）→ 分配给带 BuildKit 的 worker 构建 → 推内部 registry → 状态转 `healthy`。构建本身是一个特殊 task，复用调度与沙箱（构建容器同样断网 + 代理白名单）。

### 8.3 镜像信任与审批

**首次使用的新镜像必须管理员在后台审批**后才可执行用户代码——镜像构建阶段是独立攻击面（Dockerfile 可夹带恶意指令），运行时沙箱兜不住构建期。审批通过后镜像 digest 进入信任列表，同 digest 复用无需再审。管理后台的审批队列是核心功能之一。

### 8.4 异步环境准备

`prepare_env` 永远异步：agent 提交后立即返回，继续编码；之后通过 `get_env_status` 查询或下次 check 时自动生效。**不阻塞 agent 开发流是硬要求。**

### 8.5 健康度

每次任务执行结果回写镜像健康数据（last_success_at、success_rate_7d、total_runs）。连续 `env_error` 超过阈值 → `failing` 状态 → 调度降权 + 后台告警，提示需要修复。

## 9. GC 与资源回收

| 对象 | 策略 |
|---|---|
| worktree 环境（target volume、重建缓存） | 超过 N 天（默认 14）无任务 → 释放；磁盘水位紧急 GC 时按 LRU 提前释放 |
| CAS blob | 引用由 task_blob_refs 派生（不存冗余计数字段）；无引用 + TTL（30 天）→ 删除 |
| 全量日志 | zstd 压缩，默认保留 7 天 |
| 镜像 | 保留最近成功 N 个 + 被引用的；其余按 TTL 清理（保留 provenance 元数据） |
| 本地索引（rc-agent） | path 不存在即删 + 30 天 TTL |
| worker 离线 | 超过阈值（默认 24h）无心跳 → 标记 offline，其独占缓存 volume 进入可回收队列 |

## 10. 语言适配器

### 10.1 接口

```rust
trait Adapter {
    fn detect(&self, repo_root: &Path) -> Option<BuildProfile>;   // 自动探测
    fn default_exclude(&self) -> &[&str];                          // 同步排除清单，如 ["target/"]
    fn check_command(&self, profile: &BuildProfile) -> CommandSpec;
    fn parse_diagnostics(&self, stdout: &str) -> Vec<Diagnostic>;  // 结构化解析
    fn cache_config(&self, profile: &BuildProfile) -> CacheConfig; // sccache env 等
    fn relevant_env(&self) -> &[&str];   // profile 解析阶段吸入哪些宿主机 env（如 RUSTFLAGS）；
                                         // 吸入后随 resolved profile 整体哈希进指纹（§5.1）
}
```

### 10.2 Rust（首个实现）

- 探测：`Cargo.toml`（workspace 成员）、`rust-toolchain.toml`、`.cargo/config.toml`（默认 target）、`build.rs` 常见系统依赖提示；
- check：`cargo check --message-format=json`（stdout JSON 行，人类输出走 stderr）；
- 解析：JSON diagnostic → `level / code / message / file:line / rendered`（rendered 去 ANSI）；
- 缓存：`RUSTC_WRAPPER=sccache` + registry volume。

### 10.3 C/C++ / Go（后续）

- C/C++：gcc/clang `-fdiagnostics-format=json`；探测 CMakeLists / Makefile / compile_commands.json；
- Go：`go build -json`；探测 go.mod；
- **自定义命令兜底**：profile 中任意命令（`make check`）输出格式不可预知 → 错误模式正则提取 + 日志尾部 N 行作摘要，细节走分页日志。

## 11. 结果分级与日志

三级输出，token 成本逐层递增、按需索取：

- **L0 结论**：`success` / `N errors, M warnings`（一句话）；
- **L1 结构化摘要**（check 默认返回）：前 N 条 diagnostic（默认 10，可调），每条 `file:line + message + 短 rendered`；
- **L2 全量日志**：server 端 zstd 存储，**必须带截取参数**才能取——`get_log(task_id, offset, limit, grep?, tail?)`，禁止无参全量返回（动辄数万行）。

MCP 返回中始终附带 `task_id` 与 `result.kind`，infra_error 附自动重试记录。

## 12. MCP 接口（rc-agent 暴露给编码 agent）

| 工具 | 参数 | 返回 | 说明 |
|---|---|---|---|
| `check` | `path`, `task?`(check/build/test/clippy), `command?`, `wait_secs?` | 结果（L0+L1）或 `{pending, task_id}` | 默认短等待 3-5s，增量 check 多数同步返回；超时转异步 |
| `get_result` | `task_id` | 同上 | 轮询，token 成本极低 |
| `get_log` | `task_id`, `offset`, `limit`, `grep?`, `tail?` | 日志片段 | 强制分页 |
| `get_build_profile` | `path` | profile + 健康元数据 | 进新项目先问一句 |
| `list_envs` | `query?`(如 "rust protoc"), `arch?`, `target?` | 镜像列表 + 健康度 | 复用判断 |
| `prepare_env` | `dockerfile` 或 `image`, `project`, `reason?` | `{env_id, status}` | 异步，永不阻塞 |
| `get_env_status` | `env_id` | 构建进度/健康度 | |
| `list_workers` | - | 资源池概况 | 诊断用 |

错误约定：控制面不可达时明确报错并**建议 agent 本地执行** `cargo check`，不假装成功、不无限重试。

## 13. 服务间协议（rc-server ↔ rc-worker / rc-agent）

gRPC（tonic），核心服务：

```proto
service AgentApi {           // rc-agent → server
  rpc SyncBlobs(stream BlobReq) returns (stream BlobResp);   // 对账 + 上传
  rpc SubmitTask(SubmitReq) returns (TaskHandle);
  rpc GetTask(TaskQuery) returns (TaskStatus);               // 轮询/订阅
  rpc GetLog(LogQuery) returns (LogChunk);
  rpc UpsertProfile / GetProfile / ListEnvs / PrepareEnv ...;
}

service WorkerApi {          // worker → server 长连接
  rpc Channel(stream WorkerEvent) returns (stream ServerCmd);
  // WorkerEvent: 心跳(stats)、任务状态回报、镜像构建回报
  // ServerCmd: 派任务、取消任务、drain、构建镜像
}
```

认证：agent token（控制面签发，存 rc-agent 配置）与 worker 证书分离；所有事件带 `agent_session` 用于 supersede 作用域与审计。

## 14. 管理后台（React SPA）

管理后台是独立前端工程，面向管理员与团队成员，要求**精美、信息密度高、实时监控能力强**。

### 14.1 技术栈与部署

- **React 18 + TypeScript + Vite**；UI 用 **Tailwind CSS + shadcn/ui**（精致且可定制）；图表用 **ECharts**（运维大盘场景强于 Recharts）；数据层 **TanStack Query**；
- **实时推送走 SSE**（Server-Sent Events）：worker 心跳、任务状态变更、队列深度 1-2s 级刷新；SSE 比 WebSocket 简单，单向推送足够，断线自动重连；
- 构建产物由 rc-server 内嵌托管（`rust-embed`），**单二进制部署**，无独立前端部署物；开发期 Vite dev server 代理到 rc-server；
- 前端只走 **Admin REST/JSON API**（axum 提供，与 gRPC 并列），不碰 gRPC-web。

### 14.2 认证与权限

- 管理员账号密码 + session cookie（v0），预留 OIDC；
- 两角色：`admin`（全部操作，含镜像审批、worker 上下线、token 签发）与 `viewer`（只读，供团队成员查看任务与监控）；
- 操作类 API 全部要求 admin，审计日志记录操作者与时间。

### 14.3 页面清单

| 页面 | 内容 |
|---|---|
| **Overview 大盘** | 核心指标卡（运行中任务、队列深度、在线 worker、今日成功率、缓存命中率）+ 实时事件流 + 关键趋势图 |
| **Workers** | 列表（状态/负载/磁盘/版本）+ 详情页（CPU/磁盘曲线、运行中任务、本地缓存 volume 清单）；操作：drain / 下线 / 生成 enrollment token |
| **Tasks** | 按 project/worktree/agent/状态/结果类型过滤；详情页：结构化诊断、**日志查看器**（虚拟滚动 + grep + ANSI 渲染 + tail 跟随）、阶段时间线瀑布图、重试记录 |
| **Images** | 健康度（最近成功"xx 小时前"、成功率趋势、使用方）；**审批队列**：Dockerfile diff 查看、provenance、构建日志、批准/拒绝 |
| **Profiles** | Build Profile 列表与详情（项目 → 命令/镜像/健康度映射），支持手动修正 |
| **Storage** | CAS 总量、blob 计数、日志占用、GC 记录与水位趋势 |
| **Settings** | 管理员账号、enrollment token 管理、GC 策略、白名单代理配置、告警 webhook |

## 15. 监控与可观测性

监控是**一等公民**，目标是"不登机器就能回答：系统现在健康吗、慢在哪、缓存有没有生效"。

### 15.1 指标采集三层

1. **内置时序（开箱即用，零依赖）**：rc-server 内存 ring buffer 保存近期原始点，定期 rollup 进 SQLite（1min 粒度存 7 天，1h 粒度存 90 天）。内建 dashboard 全部数据源于此，**不强制要求 Prometheus**。注意：worker 心跳 stats（1-2s 级）只驻内存供调度与 SSE 使用，落库仅低频 `last_heartbeat` 更新与批量 rollup——SQLite 单写者，高频写入会拖垮 API 延迟；
2. **Prometheus 导出**：rc-server 暴露 `/metrics`，worker 指标经控制面聚合转发；有现成监控栈的部署方直接接入；
3. **告警**：v0 内置告警规则（worker 离线超阈值、磁盘水位、镜像连续失败、队列积压、任务超时率突增），通知走 webhook（钉钉/飞书/Slack 通用格式）；复杂告警交给 Prometheus/Alertmanager（预留）。

### 15.2 关键指标清单

| 维度 | 指标 |
|---|---|
| 任务 | 队列深度、各阶段时长分位数（queue/sync/build/upload 的 p50/p95）、成功率、supersede 率、infra_error 率、超时率 |
| 缓存 | 任务指纹命中率、sccache 命中率（worker 上报）、CAS 对账命中率、每任务平均同步字节数 |
| Worker | CPU load、磁盘余量、并行任务数、GC 次数与回收量、镜像拉取耗时 |
| 镜像 | 构建成功率、freshness（距最近成功时长）、被引用数 |
| 系统 | API 延迟与错误率、SSE 连接数、CAS/日志存储总量与增速 |

### 15.3 任务级追踪

每个任务记录阶段时间线（`queued → syncing → building → uploading`，含各 worker attempt），Task 详情页渲染瀑布图。慢任务能一眼看出卡在排队、同步还是编译——这是定位调度与缓存问题的核心手段。

### 15.4 日志

- 构建全量日志：zstd 存 server，前端日志查看器分页拉取（§11 L2）；
- 系统日志：rc-server / rc-worker 结构化日志（tracing + JSON），worker 日志可经控制面按任务/时间窗查询（v0 保留本地 journald 查询入口即可）。

## 16. 安全模型摘要

- 代码执行：容器沙箱（§7.1），断网 + 只读白名单代理 + 限额 + 超时。**残余风险明示**：只要允许访问 crates.io/github（拉依赖必需），恶意构建脚本就可经 GET 请求编码外带数据——代理能限制方法与带宽，不能根绝外泄；
- 镜像构建：审批制（§8.3）；
- 传输：TLS；认证分 agent token / worker 证书 / admin session 三类；
- 数据：代码 blob 存于编译基础设施内，属敏感数据；部署方需自行评估加密与合规要求（v0 不做静态加密，文档明示）；
- 审计：所有任务记录 (agent_session, project, fingerprint, 命令, 镜像 digest)。

## 17. 风险与决策记录

| # | 风险/坑 | 决策 |
|---|---|---|
| 1 | 扫描期间代码撕裂 → agent 修不存在的 bug | 两阶段复核 + 不稳定拒扫（§4.2） |
| 2 | 断网沙箱 vs 拉依赖矛盾 | worker 内置白名单出口代理 + registry 缓存（§7.1） |
| 3 | 指纹漏环境维度 → 错误缓存命中 | resolved profile 规范化后整体哈希，不逐字段枚举；image 以 digest 计（§5.1） |
| 4 | infra 错误被当编译错误 → agent 误改代码 | result.kind 五分类 + infra 自动换机重试（§3.5/§6.2） |
| 5 | 多 agent 同 worktree 互相 supersede | supersede 作用域 = (worktree, agent_session, task_type)（§5.2） |
| 6 | .gitignore 猜测漏文件 → 远程报错本地不报 | git ls-files/status 为枚举事实源，非 git 降级（§4.3） |
| 7 | sccache 与 incremental 互斥 | 依赖走 sccache，本地 crate 靠 target volume（§7.2） |
| 8 | 同 worktree 并发任务互等 cargo 锁 | worker 内按 worktree 串行（§6.2） |
| 9 | symlink/大小写/mtime 边界 | 不 follow、冲突报错、hash 为判据（§4.4） |
| 10 | agent 工具超时 vs 长编译 | 默认异步 + 短等待优化（§12） |
| 11 | worker 升级/僵尸资源 | drain + label reconcile（§8.1） |
| 12 | docker socket = root | 编译机专用混部禁令；gVisor/firecracker 后续（§7.1） |
| 13 | CAS/日志撑爆磁盘 | 引用计数 + TTL + 水位紧急 GC（§9） |
| 14 | 控制面单点 | v0 接受；重启 running→pending 重排（§5.3） |
| 15 | 控制面不可达 | rc-agent 明确报错并建议本地编译（§12） |
| 16 | 非确定性构建返回过期成功 | 任务级缓存 TTL 24h + 文档明示（§5.1） |
| 17 | Dockerfile 恶意指令（构建期攻击面） | 新镜像管理员审批制（§8.3） |
| 18 | 内置时序与 Prometheus 重复建设 | 内置轻量 rollup 保证零依赖开箱即用，`/metrics` 导出供已有监控栈接入，二者互补（§15.1） |
| 19 | 前端实时推送连接泄漏/风暴 | SSE 单向推送 + 断线指数退避重连；图表数据走轮询 API，仅状态变更走 SSE（§14.1） |
| 20 | base commit 未 push / 私有仓库，worker fetch 不到 | git bundle 经 CAS 上传 + 全 L2 兜底；worker 不持上游凭据（§4.1） |
| 21 | 沙箱断网下 sccache 连不上 Redis/S3 后端 | sccache server 驻 host，容器内 UDS 通信，凭据不进容器（§7.2） |
| 22 | 同 session 的 clippy 取消排队中的 check | supersede_key 含 task_type（§5.2） |
| 23 | 被订阅任务遭 supersede，订阅者永远 pending | 有订阅者则摘链降级执行，不取消（§5.2/§5.3） |
| 24 | CAS 对账与 GC 竞态 / cas_known 过期 | 对账续租 + 任务级 pin + blob_missing 自愈补传（§4.7） |
| 25 | 重建漏删除文件/丢可执行位 → 远程本地行为不一致 | manifest 为唯一事实源（含 mode 位），重建后校验（§4.4/§7.3） |
| 26 | 白名单代理成为源码外泄通道 | 代理只读方法 + 残余风险文档明示（§7.1/§16） |
| 27 | 断连取消任务 vs 异步轮询承诺矛盾 | 断连不取消，仅 pending TTL 兜底；agent_session 为持久稳定标识（§5.2/§5.3） |
| 28 | 高频心跳写爆 SQLite 单写者 | 心跳驻内存，落库低频 + rollup 批量（§15.1） |
| 29 | submodule 内容被 ls-files 跳过 → 远程缺文件 | 递归枚举子模块，内容走 L2 CAS 同步，CAS 去重兜底（§4.3） |

## 18. 存储 Schema（rc-server, SQLite v0）

```sql
CREATE TABLE projects   (id TEXT PK, repo_url TEXT, created_at INT);
CREATE TABLE worktrees  (id TEXT PK, project_id TEXT, label TEXT, last_seen INT);
CREATE TABLE profiles   (id TEXT PK, project_id TEXT, path TEXT, adapter TEXT,
                         image TEXT, config_toml TEXT, created_by TEXT,
                         last_success_at INT, success_count INT, total_count INT);
CREATE TABLE images     (id TEXT PK, digest TEXT, dockerfile TEXT, status TEXT,
                         arch TEXT, targets TEXT, approved_by TEXT,
                         last_success_at INT, success_count INT, total_count INT,
                         built_at INT, created_at INT);
CREATE TABLE workers    (id TEXT PK, labels TEXT, capacity TEXT, status TEXT,
                         stats TEXT, version TEXT, enrolled_at INT, last_hb INT);
CREATE TABLE tasks      (id TEXT PK, type TEXT, project_id TEXT, worktree_id TEXT,
                         agent_session TEXT, fingerprint TEXT, supersede_key TEXT,
                         status TEXT, result_kind TEXT, diagnostics TEXT,
                         log_ref TEXT, stats TEXT, created_at INT, finished_at INT);
CREATE INDEX idx_tasks_fingerprint ON tasks(fingerprint);
CREATE INDEX idx_tasks_supersede   ON tasks(supersede_key, status);
CREATE TABLE cas_blobs      (hash TEXT PK, size INT, last_used INT, pinned INT);
CREATE TABLE task_blob_refs (task_id TEXT, hash TEXT);
-- 引用计数从 task_blob_refs 派生，不另存 ref_count 字段（双事实源必失同步）；
-- pinned 为租约标记（§4.7），任务终态时清除
```

迁移到 Postgres 的口子留在存储层 trait 之后，v0 不实现。

## 19. 代码结构（workspace）

```
rs_remote_compile/
├── Cargo.toml                 # workspace
├── crates/
│   ├── rc-core/               # 协议(prost)、数据模型、指纹、诊断解析、CAS 客户端
│   ├── rc-agent/              # MCP server、扫描器、索引、上传、轮询
│   ├── rc-server/             # gRPC API + Admin REST API、调度器、CAS 存储、
│   │                          # SQLite、时序 rollup、SSE 推送、内嵌前端托管
│   └── rc-worker/             # 执行器、Docker 管理、缓存、代理、reconcile
├── web/                       # 管理后台 React SPA（Vite + TS + Tailwind + shadcn/ui + ECharts）
├── docs/DESIGN.md             # 本文档
└── deploy/
    ├── worker-install.sh      # 一键安装/卸载
    └── docker-registry.yml    # 内部 registry 参考部署
```

关键依赖选型：`tonic/prost`（gRPC）、`axum`（Admin REST + SSE + 静态托管）、`rust-embed`（内嵌前端产物）、`rusqlite`（WAL）、`blake3`、`rmcp` 或手写 JSON-RPC stdio（MCP）、`bollard`（Docker API）、`ignore`（降级枚举）、`tracing`（结构化日志）、`tokio`。

## 20. 里程碑

**M1（骨架跑通）**：rc-agent 扫描 + CAS 上传；rc-server 单 worker 派任务；rc-worker Docker 执行 `cargo check`；L0/L1 结果返回。单机可演示。

**M2（增量与缓存）**：git 基线层 + 内容指纹去重 + supersede；sccache + registry/target volume；本地索引。

**M3（环境池）**：worker 注册/drain/卸载；调度打分；异步 prepare_env + BuildKit 构建；镜像健康度。

**M4（agent 体验完整化）**：Build Profile 全链路（探测/仓库配置/fleet 共享）；L2 分页日志；任务级阶段时间线。

**M5（管理后台与监控）**：React SPA 全量页面（§14.3）；内置时序 rollup + 监控大盘 + SSE 实时刷新；镜像审批队列；告警 webhook；admin/viewer 权限。

**后续**：C/C++/Go adapter、Windows worker、daemon 化 rc-agent、OIDC、Prometheus 深度集成、gVisor/firecracker、Postgres。

## 21. 已确认的开放决策

以下已在讨论中拍板，记录备查：

- 全 Rust 实现（含控制面）；
- 索引存用户级目录（方案 B，§4.5）；
- 非 git 目录降级为 ignore-walk 枚举（§4.3）；
- 仓库内 `.remote-compile.toml` 优先、控制面兜底（§3.2）；
- 新镜像管理员审批制（§8.3）；
- 管理后台为独立 React SPA（Vite + Tailwind + shadcn/ui + ECharts），产物内嵌 rc-server 单二进制部署（§14）；
- 监控为一等公民：内置时序零依赖开箱即用，Prometheus 导出可选（§15）。

v0.2 评审修订（对应 §17 #20-28）：

- supersede_key 增加 task_type 维度，并定义与指纹订阅的交互（§5.2/§5.3）；
- 指纹改为整体哈希解析后的 profile，不逐字段枚举（§5.1）；
- L1 基线增加 git bundle / 全 L2 降级链，worker 不持上游凭据（§4.1）；
- 新增 CAS 对账租约与 blob_missing 自愈（§4.7）；
- manifest 增加可执行位，明确其为工作区重建唯一事实源（含删除）（§4.4/§7.3）；
- sccache server 驻 worker host、容器内经 UDS 通信（§7.2）；
- 出口代理限制为只读方法，外泄残余风险明示（§7.1/§16）；
- agent 断连取消任务 + pending TTL；心跳只驻内存不高频落库（§5.3/§15.1）。

v0.3 评审修订（对应 §17 #27 修正、#29）：

- **断连语义弱化**：断连不取消任务，仅 pending TTL（30 min）兜底；agent_session 定义为由 rc-agent 持久化的稳定标识（§5.2/§5.3）；
- **submodule 处理**：递归枚举子模块工作区，内容一律走 L2 CAS 同步，不做 L1 mirror/bundle，CAS 去重兜底（§4.3）；
- 指纹哈希前镜像 tag 必须先解析为 digest（§5.1）；
- 文字对齐：风险表 #3/#5、§9 CAS 引用策略、§10.1 `relevant_env` 注释与正文统一。
