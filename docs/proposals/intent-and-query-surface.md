# 意图解析与结构化查询面

> 状态：草案 v2.1（kimi k3 design_review 两轮：r1 APPROVE_WITH_CHANGES → r2 APPROVE；
> 裁决见 §15，评审全文 `.omc/research/intent-query-surface-design-review.md` 与
> `…-review-r2.md`）  
> 日期：2026-07-28  
> 背景：zfc monorepo 真实接入会话（全 workspace / 子 crate / test / 缓存 / cancel /
> list_envs / get_env_status 等）暴露的 14 条使用反馈。本文档拒绝逐条打补丁，
> 把反馈归纳为五个缺失的通用机制层，并给出可实施的设计。  
> 前置：`llm-contract-mechanisms.md`（Verdict / TaskContract / BudgetGate /
> DiagDelta / Lifecycle）已覆盖「结果如何正确且省地说」；本文覆盖剩余缺口——
> **请求如何正确落地**，以及 **agent 如何在不离开结构化世界的前提下问完细节**。

---

## 0. 产品契约与不变量

remote-compile 的消费者是 LLM agent。契约不变：

> **用最少的 token、以正确的归因、回答调用者此刻的问题。**

上一轮五机制建立了 I1–I5。本轮再加五条可测试不变量：

| # | 不变量 | 机制 | 覆盖反馈 |
|---|--------|------|----------|
| I6 | path 是作用域，不只是仓库根：默认命令必须反映调用方意图的包/子树 | 机制 A 意图解析 | #1 #9 |
| I7 | 任何进入上下文的结论都带执行回执：实际 command / scope / pre_commands / cache 原因不可静默 | 机制 C 执行回执 | #1 #8 #9 |
| I8 | 结构化优先于文本：诊断可过滤、可分页；raw log 是逃逸舱，不是默认下一步 | 机制 B 结构化查询面 | #2 #3 #5 #6 #14 |
| I9 | 空状态诚实：过滤无匹配 ≠ 无数据；展示 id 必须可原样回查 | 机制 B + D | #3 #4 |
| I10 | 交互轮次也是成本：pending/冷启动给出合理 wait 与队列导航，不逼 agent 盲轮询 | 机制 E 自适应交互 | #7 #11 |

设计原则（补充 DESIGN.md §1.1）：

1. **意图误解与分类错误一样贵。** 以为 check 了 `shared-crate`，实际跑全 workspace
   并把无关 crate 的 E0063 当答案，会把 agent 送去修不存在的因果链
   （机制是**作用域过大导致结果错绑**，不是 fingerprint cache 误命中——见 §3.1）。
2. **工具描述是产品 UX。** MCP description 里写的推荐路径会塑造 agent 行为；
   推荐 `grep="error"` 等于把系统推回「啃 cargo JSON」的旧世界。
3. **list → get 必须闭环。** 展示给 agent 的任何可再查标识符，必须能原样喂回查询 API。
4. **执行语义与指纹/回执必须同源。** agent 本地算出的 plan 若与 server/worker 实际执行
   不一致，Receipt 就是谎言；wire 上必须携带 PathContext，server 权威重算必须消费它
   （评审 F1）。

---

## 1. 问题：表面 14 条，底层 5 个缺口

### 1.1 实测环境与结论摘要

| 项 | 值 |
|----|-----|
| 服务 | 生产 remote-compile 实例 |
| 场景 | zfc 大 monorepo（多 crate、extra_roots、exclude、pre_commands） |
| 主路径 | 可用；指纹缓存、增量诊断、L0 结论、cancel 均表现良好 |
| 摩擦 | 多集中在「意图→命令」与「L1 不够用时被迫下沉 L2」 |

做得好的能力（**必须保留**）：L0 结论 + 归因、结构化诊断、诊断增量、
`get_result(wait_secs)` 长轮询、指纹缓存、include/exclude/extra_roots、
清晰错误信息、双槽并行。

### 1.2 反馈 → 缺口映射

| # | 实测痛点 | 表面现象 | 真正缺口 |
|---|----------|----------|----------|
| 1 | monorepo 下 `path` 不自动 scope 包 | 无 `command` 时仍跑全 workspace | **意图未解析** |
| 2 | footer 建议 `get_log(..., grep="error")` | crate 名含 error → 噪音爆炸 | **逃逸路径被当主路径** |
| 3 | get_log 无匹配文案 | 写成「没有日志 / 缓存命中」 | **空状态撒谎** |
| 4 | get_env_status 接不住 list_envs 的 id | short hash / digest / ref 全 unknown | **资源身份未归一** |
| 5 | 结构化诊断只展示 ~10 条 | 超限只能啃 raw log | **L1 不可再查询** |
| 6 | get_log 长行中间截断 | cargo JSON spans 被砍 | **把机器日志当人话分页** |
| 7 | 默认 wait_secs=4 | 冷 monorepo 几乎总异步 | **同步策略不感知任务形态** |
| 8 | pre_commands 审批静默跳过 | 失败像代码坏了 | **执行回执不完整** |
| 9 | profile 里 test 绑死某 crate | path 与 command 语义脱节 | **作用域非一等公民** |
| 10 | exclude 每次刷警告 | 长期配置下脱敏失效 | **收敛为 Critical compact 一行**（见 §1.4；不追求沉默） |
| 11 | 单 worker 排队 | 缺 queue_depth 行动提示 | **交互导航不足** |
| 12 | 源码与已部署 binary 不同步 | 改工具看错代码 | 版本可观测（工程卫生） |
| 13 | CLI 不能传 command | 与 MCP 能力不对齐 | 接口对齐 |
| 14 | 成功路径刷大量 warning | error 预算被挤占 | **预算未按决策价值排** |

附带的仓库真实问题（非工具 bug）：全 workspace check 因
`rrd-load-balancer` 缺 `ws_token` 长期红灯——加重 #1 的体感，但工具不应靠
默认藏掉根路径的全量语义来「变绿」。

### 1.3 现状代码锚点（病根位置）

| 行为 | 位置 | 现状 |
|------|------|------|
| 默认命令永远 `--workspace` | `rc-core/src/adapter.rs` `RustAdapter::command_for` | 不读 path / member |
| `path` 只用于找仓库根 | `rc-agent/src/engine.rs` `resolve_root` | 不进入命令合成 |
| 显式 command 才进 wire | `engine.rs` `command_override: req.command…` | 默认命令 **不过** SubmitTaskReq |
| server 权威重算命令 | `rc-server/src/app.rs` ~272–284 | `command_for(&Default::default(), …)`，无 PathContext |
| worker parser 门控重算 | `contract.rs` `command_is_default_resolved` | 凭 profile 重建字符串比对，无 PathContext |
| publish 物化 command | `engine.rs` `publish_profile` | 把 `resolution.command` 写入 `profile.tasks` |
| supersede 键 | `ids::supersede_key(worktree, session, task_type)` | 无 scope 维度 |
| delta 基线 | `store::resolve_baseline(project, worktree, task_type)` | 无 scope 维度 |
| 失败 footer 推 grep=error | `engine.rs` `format_result` | 固定字符串 |
| get_log 0 行统一文案 | `mcp.rs` `tool_get_log` | `total_lines==0` 即「没有日志」 |
| get_task wait 硬截 60s | `grpc_agent.rs` `wait_secs.min(60)` | 与「建议 90」冲突 |
| get_env_status 按 id 直查 | server `get_image` 路径 | 无 multi-ref resolve |

### 1.4 与上一轮机制的边界

| 已有机制 | 已解决 | 本轮必须声明的耦合（评审闭合） |
|----------|--------|--------------------------------|
| Verdict v2 | 归因 + 证据 | pre_commands skip 可作为 PROJECT_CONFIG 证据源（精度优先） |
| TaskContract | task 默认命令/env/摘要 | `default_command` 吃 PathContext；**`command_is_default` 布尔随提交下发**，worker 不再重算（F2） |
| BudgetGate | 响应字节上限 | severity-first packing 插入 Diagnostics 槽 |
| DiagDelta | 新增/基线/已修复 | **auto 基线键增加 scope**；跨 scope 视同 none，禁止跨 scope 报 fixed/new（F5） |
| Lifecycle | 进度/取消/历史参考 | Adaptive wait / queue 文案；**server wait 上限与建议一致**（F7） |
| Notice | 通知去重 | `scope_mismatch` = **Critical**；exclude 保持 Critical compact（#10 **不追求沉默**，见 F14） |

---

## 2. 目标与非目标

### 2.1 目标

1. **主路径 1–2 次工具调用完成「改 → check → 看错」闭环**；默认不进入 cargo JSON。
2. `check(path=子crate)` 在无显式 `command` 时，默认只编译该 package（或等价作用域），
   并在结果中**明示** resolved scope/command；**server/worker 实际执行与 Receipt 一致**。
3. 诊断超 L1 展示上限时，agent 用 **结构化分页** 继续看，而不是 `get_log`。
4. 所有 list 展示的资源标识可原样 get；所有「空」结果语义可区分。
5. pending / 冷启动场景给出 `suggest_wait_secs` 与队列信息，降低盲轮询。

### 2.2 非目标

- 不解决 monorepo 内真实编译错误（如 `ws_token`）；只避免工具把无关错误错绑到 path。
- 不引入新的「智能修代码」或自动改 profile；profile 仍由人/agent 显式写。
- 不做跨语言通用 package manager 抽象的大一统——Adapter 内解析，契约层只认
  PathContext / EffectivePlan。
- 不把 raw log 做成无限灌入通道（BudgetGate 不变量保留）。
- 本期不做项目级「存量 warning baseline 文件」固化。
- **本期不做 scoped 子树 manifest 指纹**（兄弟 crate 改动仍使 scoped check 缓存 miss；
  与 DESIGN §5.1「宁可多编译」一致，见 §3.6）。
- **本期 intent scope 不改变容器 cwd**（见 §3.5）。

### 2.3 成功度量（可观测）

| 指标 | 含义 | 目标方向 |
|------|------|----------|
| `explicit_command_rate` | check 带手写 command 的比例 | monorepo 日常应下降 |
| `scope_mismatch_rate` | path 与最终 command 作用域不一致 | 可解释的 Critical Notice，非静默 |
| `post_verdict_log_grep_error_rate` | 终态后短窗 `get_log` 且 grep≈error | 显著下降 |
| `diagnostics_page_rate` | 使用 `get_diagnostics` | 替代上一指标 |
| `env_status_unknown_rate` | get_env_status 返回 unknown | → 0（对 list 给出的 ref） |
| `avg_tools_per_fix_loop` | 一次修编译错误的工具调用数 | 趋向 1–2 |
| `agent_server_fp_match_rate` | scoped 提交 agent 指纹 == server 指纹 | → 100%（F1 回归哨） |

注：`cache hit 秒回` 的前提是 **同 scope 且 manifest 未变**；跨兄弟 crate 改动导致
scoped miss 是接受的取舍（§3.6），度量解读时勿与「scope 实现失败」混淆。

---

## 3. 机制 A：意图解析（Intent Resolution）— I6

### 3.1 病根

MCP `path` 的产品语义在 agent 心里是「我关心的代码在哪」，实现语义却是
「从哪上溯到仓库根并同步」。命令层永远 `--workspace`（或 profile 写死的 `-p 某包`），
造成 **silent scope expansion / silent scope hijack**：

```
check(path=.../shared-crate, task=check)   # 无 command
→ cargo check --workspace ...
→ 结果含无关 crate 的 E0063，被当成 path 的答案
→ agent 以为 shared-crate 坏了
```

（v1 曾误写「cache hit 到无关错误」：manifest 已变时指纹会变，是**新跑的宽 scope
结果错绑归因**；cache hit 只在 manifest 未变时发生，此时错误本就是同一批。——F16）

### 3.2 数据模型：PathContext + EffectivePlan

```protobuf
// additive — 挂点见 §9.1
message PathContext {
  string intent_path = 1;       // 调用方原始 path（规范化后）
  string repo_root = 2;         // agent 侧绝对路径；server 可只存相对信息
  string relative_path = 3;     // 相对 repo_root
  ScopeKind scope = 4;
  repeated string packages = 5; // adapter 解析出的 package 名（可空）
  // workdir 不由 intent 决定，见 §3.5。本字段若保留仅供展示，不得驱动 worker。
  string resolve_note = 7;      // 如何解析到的 / 为何退回 workspace
}

enum ScopeKind {
  SCOPE_UNSPECIFIED = 0;
  SCOPE_WORKSPACE = 1;
  SCOPE_PACKAGE = 2;
  SCOPE_PROFILE_OVERRIDE = 3;   // profile [tasks] 覆盖了命令
  SCOPE_EXPLICIT_COMMAND = 4;   // 请求级 command 覆盖
  // 不预留 SCOPE_PATH 死枚举（上一轮 F17 同类裁决）；未来需要时 additive 加号
}

message EffectivePlan {
  PathContext path = 1;
  string task = 2;
  string command = 3;           // 最终执行行（与 server/worker 一致）
  bool command_is_default = 4;  // 契约/adapter 默认（含 intent scope），非人写覆盖
  string profile_source = 5;    // "repo_toml" | "server" | "detect" | "request"
  PreCommandsStatus pre_commands = 6;
  string cache_key_note = 7;
  string scope_hash = 8;        // 短哈希，供 supersede / delta 基线键（见 §3.7）
}

message PreCommandsStatus {
  uint32 total = 1;
  uint32 ran = 2;
  uint32 skipped = 3;
  string skip_reason = 4;       // "pending_approval" | "disabled" | ""
  repeated string skipped_commands = 5; // 上限 5 条，过预算门
}
```

### 3.3 解析规则（有序，先命中先赢）

对 Rust adapter（其它 adapter 实现同一 PathContext 接口，可退化）：

| 序 | 条件 | scope | 默认命令形态 |
|----|------|-------|--------------|
| 0 | 请求带非空 `command` | EXPLICIT_COMMAND | 原样；**不再**追加 `-p`；`command_is_default=false` |
| 1 | profile `[tasks.<task>]` 存在 | PROFILE_OVERRIDE | profile 命令；`command_is_default=false`；若与 path 推导的 package 不一致 → **Critical** Notice `scope_mismatch`（identity = profile_command ‖ derived_packages） |
| 2 | `relative_path` 落在某 workspace member 包内（含包根） | PACKAGE | `cargo <verb> -p <name> --all-targets … --message-format=json`（flags 与现网一致；test/clippy 用各自契约 flags）；`command_is_default=true` |
| 3 | path 即 workspace 根，或无法映射到唯一 member | WORKSPACE | 保持现网 `--workspace` 默认；`command_is_default=true` |
| 4 | 非 cargo workspace / 探测失败 | WORKSPACE 或 adapter 缺省 | `resolve_note` 说明；**禁止猜测 package 名** |

**package 解析来源（确定性，含 exclude——F6）**：

1. 读根 `Cargo.toml` 的 `[workspace].members`；
2. **glob 展开后减去 `[workspace].exclude` 模式**（exclude **只**作用于 glob，
   **不**过滤显式列出的 member——与 cargo 语义对齐）；
3. 每个剩余 member 目录的 `package.name`（virtual workspace 无 root package 时仅 members）；
4. `intent_path` 规范化后，找「最长前缀匹配」的 member 目录；
5. **自校验**：解析出的 package name 必须仍在步骤 2–3 的清单内；校验失败 →
   退回 WORKSPACE + `resolve_note`（防「错匹配」生成必败 `-p`）；
6. 并列同长 / 无匹配 → 不猜，走规则 3。

`profile.path` 与 MCP path：

- MCP path 推导 package scope（命令维度）；
- profile.path **唯一**决定容器 workdir（现网 `subproject_workdir`）；
- MCP 已给出更深层 path 时，不覆盖 profile.path 的 workdir；
- 冲突可观测：Receipt 写 `profile.path=… (workdir); scope from MCP path=…`。

### 3.4 Wire 契约（F1+F2 闭合 — S1 前置）

现网断裂点：agent 只把**显式** command 放进 `command_override`；默认命令不过 wire；
server 用 `command_for(&Default::default(), task_type)` 重算（丢 target/features 与
PathContext）；worker 用字符串重算判 `command_is_default`。

**写死（非可选）**：

1. `SubmitTaskReq` **必须**携带 `PathContext path_context`（agent 解析结果，供 server
   输入）。`command_is_default` 与最终 `command` 的**权威方是 server**：server 用
   同一 `resolve_command` 重算后写入 `TaskAssignment`；agent 可预计算同名字段仅作
   交叉校验与本地 cache key，**不得**单独充当 worker 门控来源（r2 R2-F1）。
2. agent 与 server 共用 **同一** rc-core 函数：
   `resolve_command(profile, task, path_context, command_override) -> (CommandSpec, EffectivePlan)`。
3. server 命令推导序：**显式 override > profile.tasks > adapter.default(path_context)**；
   权威重算 **必须消费** 提交的 PathContext；不得 `Default::default()` 空 profile。
4. server 指纹 / canonical / TaskAssignment.command / TaskAssignment.command_is_default
   **全部**基于 server 侧重算结果；与 agent 预计算不一致时：**以 server 为准并记
   metrics**，PathContext 齐全时不一致率应 → 0（回归哨 `agent_server_fp_match_rate`）。
5. worker **不再重算**默认命令字符串；`command_is_default` 只读 assignment 下发的
   **server 权威布尔**；TestContract parser 门控只读该布尔。
6. 删除一切「可选：纯解析，仅 agent 本地」措辞——**PathContext 必须随提交传输**；
   命令与 `command_is_default` 以 server 解析为准。

端到端验收：

- `check(path=crateA)` 的 server 指纹 == agent 指纹，且 worker 执行行含 `-p crateA`；
- `check(path=crateA)` 与 `check(path=crateB)` 指纹不同（防跨 crate 服务端缓存串味）；
- scoped 默认 `task=test` → `command_is_default=true` → TestSummary 仍产出。

### 3.5 cwd 不变量（F9）

> **Intent scope 只影响命令（`-p` / flags），永不改变容器 cwd。**  
> workdir 仍 **唯一** 由 `profile.path` 决定，并已进入 canonical（现网）。

禁止把 `PathContext` 的展示字段接进 worker workdir。若未来需要 path-scope cwd，
必须：经 `profile.path` 表达 → 进 canonical → 进 Receipt；单独开设计，本期不做。

### 3.6 指纹与缓存取舍（F15）

- resolved `command`（因而 scope）进入 canonical → scope 变化不得命中旧 workspace 结果。
- `manifest_root_hash` 仍覆盖 **全量** 同步内容：兄弟 crate 改动会使 scoped check
  **缓存 miss**。这是有意取舍（DESIGN §5.1 宁可多编译），**非回归**。
- 不在本期做「scoped 子树 manifest」；写入后续方向。度量「秒回」时注明前提：
  同 scope + 全量 manifest 未变。

### 3.7 supersede 与 delta 基线的 scope 维度（F4+F5）

| 机制 | 现网键 | S1 后 |
|------|--------|-------|
| supersede | `(worktree, session, task_type)` | 增加 `scope_hash`（EffectivePlan.scope_hash） |
| DiagDelta auto 基线 | `(project, worktree, task_type)` | 增加 `scope_hash`；**无同 scope 历史 → 视同 none** |
| DiagDelta last_success | 同上 + success | 同样限同 scope |

`scope_hash = blake3_short(scope_kind ‖ sorted(packages) ‖ command_is_default ‖ normalize(command 或 profile task key))`  
具体规范化函数放 rc-core 单点，agent/server 共用。

规则：

- 同 session `check(A)` 后 `check(B)`：**不得** supersede 彼此的排队任务；
- 同 scope 连续 check：仍 supersede（「新代码取代旧代码」）；
- 跨 scope 的 auto delta：**不得**报告 fixed_count / 把兄弟诊断当 new（F6.3 再现）。

### 3.8 publish_profile 规则（F3）

首次 success 且 server 无 profile 时，现网会把 `resolution.command` 写入
`profile.tasks[task]`，在 S1 后会把 `-p <第一个跑绿的 crate>` **冻结成 fleet 默认**。

**写死**：

- 若 `command_is_default == true`：**不得**把该 command 物化进 `profile.tasks`；
  publish 只上传 adapter/image/target/features/env 等非 intent 字段；
- 仅当 `command_is_default == false`（显式 command 或人写 profile tasks 已存在）时，
  才允许 tasks 进入 fleet profile；
- 测试：scoped success 后 `get_build_profile` 的 tasks 不含 `-p <that crate>`。

### 3.9 Adapter API

```rust
fn resolve_path_context(
    &self,
    repo_root: &Path,
    intent_path: &Path,
) -> PathContext;

fn command_for(
    &self,
    profile: &BuildProfile,
    task: TaskType,
    path: &PathContext,
) -> CommandSpec;
```

`TaskContract::default_command` 从 `TaskFlags` 携带 PathContext。  
**唯一入口** `resolve_command(...)` 同时产出 command 与 `command_is_default`。

### 3.10 测试（I6 机械化）

- member 内 path → `-p that_member` 且含 `--all-targets`（check），不含误加的 `--workspace`；
- workspace 根 → `--workspace`；
- 显式 command 不被二次改写；`command_is_default=false`；
- profile tasks 覆盖 → PROFILE_OVERRIDE + Critical scope_mismatch（若与 path 不一致）；
- glob + exclude 不把 excluded 目录当 member；错匹配自校验回退 WORKSPACE；
- 无法映射 → WORKSPACE + resolve_note 非空；
- 同 manifest 仅 scope 不同 → fingerprint 不同；agent fp == server fp；
- scoped success publish 后 profile.tasks 无 `-p`；
- 跨 scope supersede 不互杀；同 scope 仍 supersede；
- 跨 scope auto delta 无 fixed/new；
- scoped 默认 test → parser 启用、TestSummary 产出。

---

## 4. 机制 B：结构化查询面（Structured Query Surface）— I8 / 部分 I9

### 4.1 病根

L1 写死截断且不可再查询；超限 footer 把 agent 推到 L2 文本搜索；L2 主体是
cargo JSON，子串 `error` 命中 `thiserror` 等噪音。

### 4.2 层级

| 层级 | 职责 | 默认是否进上下文 |
|------|------|------------------|
| L0 | 过/不过 + 归因 + Execution Receipt | 是 |
| L1 | error-first 结构化诊断；warning 折叠 | 是，BudgetGate 内 |
| L1.5 | `get_diagnostics` 过滤/分页 | 按需 |
| L2 | `get_log` 逃逸舱 | 仅结构化不足时 |

### 4.3 `get_diagnostics`（F8 闭合）

**实现路径（写死）**：不新增 server RPC。MCP 工具在 agent 侧：

1. `get_task(task_id, wait_secs=0, baseline=…)`；
2. 读 `result.diagnostics` / `diag_delta`；
3. 本地按 severity / code / file_prefix / only_new 过滤；
4. 按 offset/limit 分页；过 BudgetGate 输出。

```text
get_diagnostics(
  task_id,
  severity? = "error" | "warning" | "all",   # 默认 error
  offset? = 0,
  limit? = 20,                                 # hard max 100
  code? = "E0063",
  file_prefix? = "zf-web/",
  only_new? = false,
  baseline? = "auto" | "none" | "last_success" | <task_id>  # only_new 时生效；默认 auto
)
```

**分页语义**：

| 项 | 定义 |
|----|------|
| total | **stored** 诊断条数（过滤后），不是含 truncated 的真实 rustc 总数 |
| truncated | 展示 `(+N truncated not stored)`；无法分页到达未存储部分 |
| offset 越界 | `(no more stored diagnostics; +N truncated not stored — use get_log if needed)` |
| only_new | 仅当 baseline 解析成功且 delta 可用；否则明确「no baseline / delta unavailable」，不装成 empty success |

数据源：任务已解析 diagnostics（≤50 条现网上限）+ delta；**不**重扫 raw log。  
cache hit 且结果带 diagnostics 时同样可查。

### 4.4 `get_log` 诚实空状态

| 条件 | 文案 |
|------|------|
| 无日志体（未执行 / GC / cache hit 未存 log） | `(no log stored: <reason>)` |
| 有日志且 grep 非空，匹配 0 行 | `(0 lines matched grep="…"; total_lines=N; matched_lines=0)` |
| 正常分页 | 现网 header + 行 |

```protobuf
message LogChunk {
  // 现有字段保留
  uint64 matched_lines = 10;  // grep 后匹配总数；无 grep 时 == total_lines
  string empty_reason = 11;   // "no_log" | "no_match" | ""
}
```

server grep 后须同时返回 **过滤前** total 与 matched（或 empty_reason），避免
`total_lines==0` 被 agent 误判为无日志。

**工具描述**：删除「定位问题优先用 `grep="error"`」；改为优先 `get_diagnostics`；
若必须 grep，优先 `error[E` 或具体 code/字段名。

### 4.5 get_log 渲染口径（F13）

本期对 §4.5 cargo JSON → human summary 的 **默认改写降级为非目标**：

- footer 已改指 `get_diagnostics`；get_log 主诉是 panic 栈 / 链接器全文等非 JSON；
- 若后续做渲染，必须写死：**grep 恒按 raw 行匹配；行号/offset 恒为 raw；
  渲染仅 1:1 替换展示；证据直达默认 `raw=true`**。

BudgetGate 单行省略规则不变；`raw=true` 仍受响应总预算约束。

### 4.6 L1 error-first packing（#14）

1. 先放 error（及 delta 新增 error）；
2. warning 折叠：`W×15 (dead_code×10, unused_variables×5)`；
3. 详情 → `get_diagnostics(severity="warning")`；
4. 成功 0 error 时 L1 默认仅折叠行。

### 4.7 失败 footer

```
需要细节: get_diagnostics(task_id="…", severity="error", offset=0, limit=20)
```

仅当 diagnostics 为空且需要 log 证据时：

```
需要细节: get_log(task_id="…", offset=<evidence.line_no>, limit=30, raw=true)
```

---

## 5. 机制 C：执行回执（Execution Receipt）— I7

### 5.1 病根

结果不声明系统实际执行了什么 → 意图/环境/配置误解都像代码坏了。

### 5.2 呈现

**每次终态结果（含 cache hit）必显**；`scope=package` 时 **headline 内嵌限定**（F12）：

```
✓ zf-web: 15 warnings [成功, scope=package]
task_id=t-…  (cache hit)  build=8200ms
scope=package:zf-web  command=cargo check -p zf-web --all-targets --message-format=json
profile=repo_toml  pre_commands=skipped(pending_approval)×2
```

失败 + pre_commands skipped 时 Critical Notice：

```
⚠ pre_commands skipped (pending approval): cargo run -p xtask codegen
  若错误像生成物/path 缺失，优先查环境/profile，不要先改业务代码
```

归因（精度优先）：

- `pre_commands.skipped > 0` 且 **零** 结构化 compile error，症状像 missing file →
  倾向 PROJECT_CONFIG，证据引用 skip_reason；
- 已有明确 E0xxx 指向业务源码 → 仍 CODE，Receipt 仍显示 skipped。

### 5.3 存储

`TaskResult.effective_plan`（additive）；cache 回放必须带上（与
`record_cache_hit` 复制 result_json 兼容）。

pre_commands 的 skipped 列表：server/profile 路径须暴露 **具体命令**（不仅 bool）；
见 §9.1 `ProfileResp` / 任务结果字段。S5 含采集，S1 Receipt 在无数据时写
`pre_commands=unknown` 而非谎称 `ran`。

---

## 6. 机制 D：资源身份归一（Identity Resolution）— I9

### 6.1 病根

list 展示串无法被 get 原样消费。

### 6.2 resolve(ref)

```text
resolve_env(ref) accepts:
  1. 精确 env_id / DB 主键
  2. 完整 image reference（含 @sha256:…）
  3. repository 路径 + tag
  4. 唯一 short prefix（如 0f5446c3）
歧义 → candidates[]，禁止 silent pick
未命中 → not_found + 引导看 list 的 env_id=
```

**list_envs 强制格式**：

```
env_id=<canonical>  image=<ref>  status=healthy  success_rate=…
```

### 6.3 实现

server store `find_image_by_ref`；gRPC GetEnvStatus 走 resolve；agent 翻译 candidates。

---

## 7. 机制 E：自适应交互预算（Adaptive Interaction）— I10

### 7.1 病根

默认 wait=4 对冷 monorepo 几乎总 pending；建议 wait=90 却被 server `min(60)` 截断（F7）。

### 7.2 拍板（F7）

**提高 server 长轮询上限到 120s**（`wait_secs.min(120)`），与 monorepo 建议对齐。

| 项 | 值 |
|----|-----|
| server 硬上限 | **120s**（原 60；需评估连接占用，metrics 观察） |
| tool description 建议 | 冷 monorepo `wait_secs=60~120` |
| `suggest_wait_secs` | `clamp(history_build_ms_p50/1000 * 1.2, 15, 120)`；无历史则 60 |
| 默认 `default_wait_secs` | 仍短（配置可调，默认 4）；**不**在默认上强迫堵 MCP |
| cache 快路径 | 本地指纹命中仍同步返回 |

pending 响应（字段见 §9.1；示例与公式一致：p50=85000 → suggest=102）：

```
pending task_id=…
phase=queued  queue_depth=3  running=2  capacity=2
history_build_ms_p50=85000  suggest_wait_secs=102
next: get_result(task_id="…", wait_secs=102)
```

### 7.3 CLI 与版本（#12 #13）

- `rc-agent check --command …` 与 MCP 对齐；
- `rc-agent status` 与 MCP `initialize.serverInfo.version` 含 `0.x.y+<gitsha>`。

---

## 8. Agent 工作流：现状 vs 目标

### 8.1 现状

```
改代码
  → check(path=子crate)              # 常变成全 workspace
  → wait 4s → task_id
  → get_result(wait=60~120)          # 90 被截成 60 时可能多一轮
  → ≤10 条诊断 → get_log grep=error  # 噪音 / 假无日志
  → 修 → check → cache hit
```

### 8.2 目标

```
改代码
  → check(path=子crate, wait_secs=90)  # 自动 -p；Receipt 明示；server 真执行 -p
  → L0（headline 含 scope）+ error-first L1
  →（可选）get_diagnostics(offset=…)
  → 修 → check → 同 scope 且 manifest 未变则 cache hit
```

---

## 9. 兼容性、proto 与迁移

### 9.1 Proto / 字段挂点清单（F10）

| 消息 | 字段 | 号（建议） | 生产方 | 消费方 |
|------|------|------------|--------|--------|
| `SubmitTaskReq` | `path_context` | 16 | agent | server `resolve_command` 输入 |
| `SubmitTaskReq` | `command_is_default`（可选预计算） | 17 | agent（交叉校验用） | server 可忽略；权威值重算后下发 |
| `TaskAssignment` | `command_is_default` | additive | **server**（`resolve_command` 权威） | worker parser 门控 |
| `TaskAssignment` | `scope_hash` | additive | server | supersede / delta |
| `TaskResult` | `effective_plan` | 15 | server/worker 完成时写入；agent cache 回放 | format_result |
| `TaskStatus` | `queue_depth` | additive | server | pending 导航 |
| `TaskStatus` | `running` / `capacity` | additive | server | pending 导航 |
| `TaskStatus` | `suggest_wait_secs` | additive | server 或 agent 本地 | pending 导航 |
| `TaskStatus` | `history_build_ms_p50` | **复用现网字段**（勿另造 `history_p50_ms`） | server | pending 导航 / suggest 公式 |
| `LogChunk` | `matched_lines` | 10 | server | get_log 空状态 |
| `LogChunk` | `empty_reason` | 11 | server | get_log 空状态 |
| `ProfileResp` | `pending_pre_commands` (repeated string) | additive | server | Receipt / get_build_profile |
| `EnvQuery` / status | resolve 语义 | — | server | get_env_status |

字段号以 `rc.proto` 落地时 `buf`/`prost` 实际分配为准；表约束的是 **语义挂点**，
实现 PR 更新号后回写本文。

旧 agent 不传 path_context → server 行为与今日一致（workspace 默认）。  
旧 worker 不识 command_is_default → 回退现网字符串比对（仅无 `-p` 时正确）。

### 9.2 其它迁移

| 项 | 策略 |
|----|------|
| 默认命令语义 | **有意变更**：子路径 → package scope。CHANGELOG 显著标注；**根 path 不变** |
| 指纹 | command 已在 canonical 则自然 miss；若 PathContext 进 hash 结构变化则考虑 ABI 注释更新 |
| supersede 键 | 新任务带 scope_hash；旧 pending 无 hash 时仅匹配「空 scope」（或保守：旧键仍按三元组，文档说明升级窗口可能多杀一次） |
| delta 基线 | 无 scope_hash 的历史任务不参与 scoped auto 基线 |
| publish | §3.8；kill-switch `intent_scope=false` 时恢复旧 publish 行为 |
| wait 上限 | 60 → 120；监控长轮询连接数 |
| kill-switch | `intent_scope`、`diagnostics_api`、`identity_resolve`、`adaptive_wait` 独立 |

---

## 10. 实施切分

| 切片 | 内容 | 验收 |
|------|------|------|
| **S1** | PathContext + wire 携带 + server 消费 + command_is_default 下发 + publish 规则 + supersede/delta scope 键 + Receipt（含 headline scope）+ cwd 不变量 + member exclude 解析 | #1 #9；agent/server fp 一致；worker 真跑 `-p`；scoped test 有 TestSummary；跨 scope 不互杀/不假 fixed |
| **S2** | get_diagnostics（agent 侧）+ footer/描述 + get_log 空状态字段 + error-first L1 | #2 #3 #5 #14 |
| **S3** | env resolve + list 打印 env_id= | #4 |
| **S4** | wait 上限 120 + pending 队列字段 + suggest_wait + 文档/description | #7 #11 |
| **S5** | pre_commands 列表进 Receipt + CLI `--command` + version/gitsha + exclude Critical compact 文案对齐 #10 | #8 #10 #12 #13 |

**依赖**：S1 必须先闭合 F1/F2/F3/F5/F9；S2 可并行于 S1 后半（仅依赖已有 diagnostics 存储）；S3 独立；S4 依赖 wait 上限拍板（已写入本文）；S5 依赖 pending_pre_commands 采集。

**优先：S1+S2。**

---

## 11. 测试与契约

### 11.1 单元 / 集成

见 §3.10；另加：

- get_diagnostics：severity/offset/limit/only_new/baseline/越界/truncated 文案；
- get_log：有 log+无匹配 / 无 log / 有匹配；
- resolve_env：id/digest/short/歧义/未命中；
- format_result：Receipt 必现；headline 含 scope；成功 warning 折叠；footer 指 get_diagnostics；
- wait：server 接受 90 并等到 90（≤120），拒绝 >120 截断。

### 11.2 Agent 契约

合成「子 crate 干净、兄弟 crate 有 E0063」：

1. `check(path=clean-crate)` → 不得出现兄弟 E0063；Receipt `scope=package`；
2. `check(path=repo_root)` → 仍可报告兄弟错误；
3. `get_log(grep="THIS_NEVER")` → matched=0 且 total_lines>0，不得称「没有日志」；
4. 连续 `check(A)` 排队中再 `check(B)` → A 不被 supersede；
5. scoped success 后 fleet profile.tasks 无 `-p A`。

---

## 12. 文档与 runbook

机制落地后 runbook 变薄：

- 删除「必须手写 `command=cargo check -p …`」主路径（保留高级覆盖）；
- 子路径默认 package scope；全量用根 path；
- 细节优先 `get_diagnostics`；
- `default_wait_secs` / 冷启动建议 ≤120；
- exclude 每次 compact 一行是有意（正确性），不是 bug。

DESIGN.md §10–§12 实现后同步。

---

## 13. 风险与裁决预案

| 风险 | 缓解 |
|------|------|
| 自动 `-p` 漏掉 workspace 级检查 | 根 path / 显式 command 仍支持；headline 标 scope=package |
| member 与 cargo 不完全一致 | exclude+自校验；失败 → WORKSPACE；metadata opt-in 后续 |
| 行为变更使依赖「全 workspace 红」的脚本变绿 | CHANGELOG；CI 用根 path |
| wire 漏传 PathContext | S1 端到端 fp 哨兵；不一致 metrics 告警 |
| publish 再冻结 `-p` | §3.8 测试锁死 |
| wait 120 占连接 | metrics；可配置上限 |
| get_diagnostics 与 delta 重复 | only_new 默认 false；L1 优先 delta 新增 |

---

## 14. 总结

zfc 反馈收敛为两句产品话：

1. **「path 表示我想检查的范围」** —— Intent Resolution + 执行回执 + **wire 同源**；  
2. **「agent 不该默认读编译器 raw 日志」** —— 结构化查询面 + 诚实空状态。

v1 机制划分正确；v2 补上评审挖出的 **执行链路闭合**（命令真到达 worker、
parser/publish/supersede/delta 与 scope 同维）。S1 在四处前置闭合前不得开工。

---

## 15. 评审裁决记录（kimi k3 / design_review）

评审全文：`.omc/research/intent-query-surface-design-review.md`  
裁决：**APPROVE_WITH_CHANGES**（BLOCKER 4 / MAJOR 6 / MINOR 5 / NIT 2）

| ID | 严重度 | 摘要 | v2 处置 |
|----|--------|------|---------|
| F1 | BLOCKER | 默认命令不过 wire；server 无 PathContext 重算 | §3.4 写死 SubmitTaskReq + 唯一 resolve_command |
| F2 | BLOCKER | worker parser 重算失败 | command_is_default 下发；worker 不重算 |
| F3 | BLOCKER | publish 冻结 `-p` 为 fleet 默认 | §3.8 command_is_default 不进 tasks |
| F4 | MAJOR | supersede 无 scope 互杀 | §3.7 scope_hash |
| F5 | BLOCKER | delta 跨 scope 假 fixed/new | §3.7 同 scope 基线 |
| F6 | MAJOR | members 缺 exclude → 错匹配 | §3.3 解析规则 |
| F7 | MAJOR | suggest 90 vs server min(60) | §7.2 上限改 120 |
| F8 | MAJOR | get_diagnostics 未定义实现/分页 | §4.3 agent 侧 get_task + 语义表 |
| F9 | MAJOR | workdir 悬空 / 指纹 | §3.5 cwd 不变量 |
| F10 | MAJOR | 无 proto 挂点清单 | §9.1 表 |
| F11 | MINOR | scope_mismatch 严重度 | Critical，§3.3 规则 1 |
| F12 | MINOR | headline 无 scope | §5.2 内嵌 |
| F13 | MINOR | get_log 渲染/grep 口径 | §4.5 降级非目标 + 若做则 raw 口径 |
| F14 | MINOR | #10 与 Critical 冲突 | §1.2/#10 改为 compact 不沉默 |
| F15 | MINOR | 全量 manifest 与 scoped 缓存 | §3.6 写明取舍 |
| F16 | NIT | cache hit 措辞 | §3.1 改正 |
| F17 | NIT | 示例缺 --all-targets；死枚举 | 示例补齐；删除 SCOPE_PATH |

### 15.2 第二轮（r2）— APPROVE

全文：`.omc/research/intent-query-surface-design-review-r2.md`  
F1–F17 全部 **CLOSED**；新发现 3 条均不阻断开工，已并入 v2.1：

| ID | 严重度 | 摘要 | v2.1 处置 |
|----|--------|------|-----------|
| R2-F1 | MINOR | `command_is_default` 权威方双源 | §3.4/§9.1：server 权威重算；agent 仅交叉校验 |
| R2-F2 | NIT | suggest 示例与公式不符 | §7.2 示例改为 102 |
| R2-F3 | NIT | `history_p50_ms` 与现网 `history_build_ms_p50` 重复 | §9.1 复用现网字段名 |

**开工结论（r2）**：S1、S2 可开工；R2-F1 随 S1 实现。

**保留不动**（评审「不建议改动」）：无法映射回退 workspace；MCP path 赢并可观测；
Receipt 随 cache 回放；LogChunk additive 空状态；resolve 禁止 silent pick；
footer 改指 get_diagnostics；kill-switch 独立；机制 E 不强迫默认加长 wait；
默认 TOML 解析不 shell-out metadata。

---

## 附录 A：五优先事项对照

| 若只做 5 件事 | 机制 | 切片 |
|---------------|------|------|
| path → crate 自动 `-p` 且 **真执行**（wire 闭合） | A | S1 |
| get_diagnostics 结构化分页 | B | S2 |
| get_log 空匹配 + 去掉有害 grep 建议 | B | S2 |
| get_env_status id 互通 | D | S3 |
| Receipt：command / pre_commands / cache 原因 | C | S1+S5 |

## 附录 B：与 llm-contract-mechanisms 关系图

```
请求入口                         结果出口                      追问
────────                         ────────                      ────
path/task/command
  → 机制A PathContext（本地）
  → SubmitTaskReq{path_context, command_is_default}   ← 必须过 wire
  → server resolve_command(同一 rc-core) + 指纹/supersede(scope)
  → worker 执行 assignment.command；parser 读布尔
  → 机制一 Verdict / 机制四 DiagDelta(同 scope 基线)
  → 机制C Receipt（与真实 command 一致）
  → 机制三 BudgetGate → L0/L1

pending → 机制E 队列导航 + wait≤120
L1 不够 → 机制B get_diagnostics（agent 侧过滤）
仍不够 → get_log（诚实空状态；禁止默认 grep=error）
资源   → 机制D resolve(ref)
```
