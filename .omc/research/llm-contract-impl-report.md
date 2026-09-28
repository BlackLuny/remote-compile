# LLM 契约化改造 — 实施报告

> 日期：2026-07-28  
> 规格：`docs/proposals/llm-contract-mechanisms.md` v2  
> 约束：叠加在既有 egress 未提交改动之上；无 git 写操作。

最终自验（第三轮后）：`cargo check --workspace` 通过；`cargo test --workspace` 全绿  
（rc-agent 127 / rc-core 192 / rc-server 125 / rc-worker 79）。

---

## 第三轮修复（核验未闭合项 R1'/R3'/R5'/R6'/R7'/R10'/R11'）

对照 `.omc/research/llm-contract-verify-gpt.md` 的 7 条 FAIL。

| # | 闭合证据（文件:行） | 测试 |
|---|---------------------|------|
| **R1'** | `diag.rs` `classify_with_exec` 经 `for_task(task_type).parse_outcome(...)` 取 TestSummary，生产路径不再直接调 `parse_test_summary` | 既有 classify fixtures 全绿 + `green_test_classify_to_format_carries_passed_count` |
| **R3'** | `engine.rs` `attach_notices` → `budget::assemble_result_with_notices` 替换 `with_note` 尾部追加；`present_notices_split` 分 Critical/Info | `budget::critical_survives_oversize_body`；`engine::critical_notice_survives_budget_attach` |
| **R5'** | `register_remediate_slot` 存 `submit.profile.env` 完整 effective；`plan_remediation` 比对 first/second effective 逐字节 | `remediation_skips_when_effective_env_already_has_jobs` |
| **R6'** | `Store::requeue` 条件 `WHERE status NOT IN (终态)` 返回 bool；`on_task_done` blob/infra 分支据 false 放弃 | `requeue_does_not_resurrect_canceled` |
| **R7'** | worker emitter `select!` dirty + `interval.tick` flush；关通道再 flush | `progress_throttle_flushes_after_window_without_more_changes` |
| **R10'** | `App.terminal_cache` 按 `task_id\0baseline` 记忆 delta+history；重复查询复用 | `terminal_delta_is_memoized_across_queries` |
| **R11'** | 绿 test 贯穿 `classify→format_result`；补救状态机 `plan_remediation`+`rem_state` 注册；双败 first task_id | `green_test_classify_to_format_carries_passed_count`；`async_remediate_state_machine_registers_and_plans`；`dual_fail_text_uses_first_task_id` |

### 第三轮后仍留档

- agent 默认仍不开 `return_on_progress`（字段就绪，不宣称游标闭环已交付）。
- fuzzy rename 文案仍不单独输出。
- 真实 docker OOM 网络 E2E 未在 CI 起容器；补救以状态机/plan 单测覆盖可达性。

---

## 第二轮修复（对抗式评审 R1–R11）

对照 `.omc/research/llm-contract-impl-review-gpt.md` 与
`.omc/research/llm-contract-impl-review-claude.md` 的裁判合并清单。

| # | 结果 | 摘要 |
|---|------|------|
| **R1** | 已修 | `command_is_default` / `command_is_default_resolved` 放 rc-core；worker 经该门控再 `classify_with_exec`；规则 0 挂 TestSummary 并渲染 `测试通过：N passed…`；规则 4 仅 test+门控摘要。单测：绿 test 计数、override 不解析、build 含 libtest 噪声不产 CODE。 |
| **R2** | 已修 | server `submit` 调 `effective_profile`（resolve_env + canonicalize_resolved），**忽略**客户端 `canonical`；存库/下发均为 effective 产物。单测：`lying_canonical_does_not_change_fingerprint`、`lying_canonical_is_ignored_by_effective_profile`。 |
| **R3** | 已修 | MCP `call_tool` 与 Tool error 唯一过 `gate_response`（硬 ≤8192）；`assemble` 预留 Critical 最后拼装；get_log schema+实现 `line_byte_offset` 行内续读（header_reserve 预留）。契约单测：硬上限、Critical 存活、续读前进。 |
| **R4** | 已修 | `resolve_baseline(..., exclude_id, before_finished_at)`；`TaskQuery.baseline` additive 下传；`get_result`/`get_task_ex` 传 mode。E2E：`second_task_in_worktree_gets_diag_delta`。 |
| **R5** | 已修 | 进程内 `RemediateSlot`：check 注册、**get_result 亦可触发**；双败用**首次 task_id** 格式化；补救 knob → `CARGO_BUILD_JOBS=2`；no-op patch 跳过。设计文档 §2.5 已改。 |
| **R6** | 已修 | `CancelTaskReq.project_id` 与 task 行比对（注释写明 token 为 fleet 级）；`complete_task`/`set_status` 条件更新；双向竞态单测。 |
| **R7** | 已修 | worker unit 进度 `phase=""`；server 仅内存 snapshot；`unit_progress` kill-switch 丢弃 unit；emitter 关闭前 flush；终态清 `App.progress`。单测：unit 不增 task_events。 |
| **R8** | 已修 | 规则 7 SIGSEGV/SIGABRT 均需 `error: could not compile`；规则 9 只扫 log_tail。fixture：test SIGSEGV/SIGABRT → UNKNOWN。 |
| **R9** | 已修 | `baseline_off` Critical + 自包含 compact；scanner 多警告聚合成单 Notice。 |
| **R10** | 已修 | delta 正则 OnceLock；裸 `:\d+:\d+` 锚定行尾；empty code 含 warning；delta/history 仅 terminal 计算。 |
| **R11** | 已修 | (1) 同 worktree 二次 check 有 delta；(2) 补救路径可达 get_result 状态机（单测 no-op/knob + 代码路径）；(3) 绿 test 含 passed 计数。 |

### 第二轮后仍留档的偏离

1. **fuzzy rename 文案**：仍只抬 `approximate`，不单独输出「疑似移动」行。
2. **agent 默认不开 `return_on_progress`**：字段与 server 游标语义已就绪，但 agent 轮询默认仍为旧长轮询终态/超时返回——**不得表述为「已交付游标闭环」**。
3. **异步 OOM 的完整网络 E2E**（真实 docker OOM → get_result → 补救）依赖 worker 环境，本轮以状态机 + 分类/补救单元与集成测试覆盖可达性；未在 CI 起容器打真实 OOM。
4. **`format_result` 未拆成完整 SlotPiece 管线**：MCP 出口用 `gate_response` 硬帽 + `assemble_result_with_notices` 保 Critical；诊断量大时仍可能压缩诊断而非分槽精细配额。

---

## PR1 — 机制一：证据化归因（Verdict v2）

### 改动文件

| 文件 | 变更 |
|------|------|
| `crates/rc-core/proto/rc.proto` | additive：`Status`/`Attribution`/`Evidence`/`Verdict`/`ExecEvidence`/`TestSummary`/`DiagDelta`；`TaskResult` 字段 11–14；后续 PR 字段一并加入 |
| `crates/rc-core/build.rs` | 新字段 `#[serde(default)]` 保证旧 result_json 可反序列化 |
| `crates/rc-core/src/diag.rs` | 规则表 `classify_facts` 重写；`parse_test_summary`；§1.3 映射；§1.7 fixture 全量 |
| `crates/rc-worker/src/docker.rs` | `remove_container` 前 inspect → `ExecEvidence`；超时路径 `worker_killed` |
| `crates/rc-worker/src/runner.rs` | `classify_with_exec`；结果写入 `verdict`/`test_summary` |
| `crates/rc-agent/src/engine.rs` | `format_result` 按 attribution 渲染 + 证据行 |
| `crates/rc-agent/src/index.rs` | `ResultCache.put` 仅缓存 `{success, compile_error}` |

### 关键决策

- 旧 `classify(...)` 保留为包装，内部自动解析 `TestSummary`，现网单测无需改签名。
- `ATTR_CODE` 仅规则 3/4；原「test 非零无诊断 → CODE」改为未知或具体规则（I1）。
- 磁盘满：规则 8 → `INFRA`（可换机重试），不再走 env_error 路径。
- SIGKILL 匹配 cargo 精确串 `(signal: 9, SIGKILL: kill)`。

### 测试

- 现网不变量绿：`timeouts_win_over_everything`、`a_compile_error_is_never_offered_a_package`、`failing_tests_are_a_code_problem`。
- §1.7 fixtures：OOMKilled、仅日志 SIGKILL、worker 超时 137、rustc SIGSEGV、test abort 无摘要、磁盘满、纯编译错误、编译+env marker 混合、测试失败摘要、全 env 诊断。

### 偏离规格

无。

---

## PR2 — 机制二：任务语义契约（TaskContract）

### 改动文件

| 文件 | 变更 |
|------|------|
| `crates/rc-core/src/contract.rs` | **新建**：`TaskContract` 四实现；`resolve_env`；denylist；补救白名单 |
| `crates/rc-core/src/fingerprint.rs` | `EXECUTOR_ABI` abi2→**abi3**；兼容测试旧指纹不命中 |
| `crates/rc-core/src/lib.rs` | 导出 `contract` 等模块 |
| `crates/rc-agent/src/engine.rs` | resolve 时合并 contract env；自动补救 1 次 |
| `crates/rc-agent/src/config.rs` | `auto_remediate` + 五 kill-switch |
| `crates/rc-agent/src/mcp.rs` | `env` / `no_remediate` / `baseline` 参数 |
| `crates/rc-core/proto/rc.proto` | `SubmitTaskReq.env = 15` |

### 关键决策

- 分层：adapter 空 map < contract default_env < profile env < 请求 env；产物写回 `profile.env` 再 canonicalize。
- `TestContract` 默认注入 `CARGO_PROFILE_TEST_DEBUG=0` 与 `CARGO_PROFILE_DEV_DEBUG=0`（产品决策，非 opt-in）。
- libtest 解析仅在「命令为本契约默认」时启用（F24）。
- 自动补救白名单封闭：`{oom_killed, sigkill_suspected_oom}`；双败以首次 verdict 为主并附第二次 task_id。

### 测试

- contract 单元：层叠、denylist、parser 启用条件、补救白名单。
- fingerprint：abi3 ≠ abi2。

### 偏离规格

1. **Server 侧未二次调用 `resolve_env` 重算 profile**：当前 agent 在提交前已把 effective env 写入 `ResolvedProfile.env` 并完成 fingerprint；server 仍权威重算 fingerprint（`compute_for`）。若恶意 agent 在 profile.env 与请求 env 字段上撒谎，worker 只消费 profile。严格「server 同源 resolve」可后续在 `admit` 路径补一行 merge。
2. **`CheckRequest.baseline` 已接线到 MCP，但 agent 尚未把 baseline 模式传到 server**；server `task_status` 目前固定 `auto` 解析基线（见 PR4）。

---

## PR3 — 机制三：BudgetGate + Notice

### 改动文件

| 文件 | 变更 |
|------|------|
| `crates/rc-core/src/budget.rs` | **新建**：字节计量、单行 400B 中间省略、8KB 分槽、`gate_log_lines` raw 续读 |
| `crates/rc-core/src/notice.rs` | **新建**：快照语义状态机；键 `(project_id, worktree_id, category)` |
| `crates/rc-agent/src/engine.rs` | `describe_*` 迁为 `collect_notices` + `present_notices`；Critical 每次 compact |
| `crates/rc-agent/src/mcp.rs` | 响应过 `gate_response`；get_log 过预算门并带 next_offset |

### 关键决策

- exclude / egress_pending / egress_refused 标 Critical（正确性相关）。
- Notice 进程内记忆，重启即遗忘（规格允许）。

### 测试

- budget：UTF-8 边界、headline 永不丢、raw 行内续读、总上限。
- notice：首次全文、Info 静默、Critical compact、跨项目不串、消失后复发。

### 偏离规格

1. **Budget 分槽在 `assemble` 中实现，但 `format_result` 尚未拆成 SlotPiece 再 assemble**；当前 MCP 出口对整段文本 `gate_response`（总上限 + 单行 elide）。headline/证据在前部，截断时优先保留开头，语义接近但未严格按 4KB/2KB 槽配额裁剪诊断。
2. **get_log 的 server 侧 `line_byte_offset` 行内续读**仅 agent 预算门实现；server `LogChunk` 字段已 additive 预留。

---

## PR4 — 机制四：诊断 delta

### 改动文件

| 文件 | 变更 |
|------|------|
| `crates/rc-core/src/delta.rs` | **新建**：`normalize_spans`（仅 span）、strict/fuzzy、approximate、truncated 不报 fixed |
| `crates/rc-server/src/store.rs` | `resolve_baseline`（auto/last_success/task_id） |
| `crates/rc-server/src/app.rs` | `task_status` 读时计算 `DiagDelta`（可 kill-switch） |
| `crates/rc-agent/src/engine.rs` | 渲染增量块；新增诊断优先占 max_diagnostics |

### 关键决策

- 基线键：`project_id + worktree_id + task_type`；跨 worktree 不借用。
- 文案「基线已有」，不断言「非本次改动引入」。

### 测试

- 行号漂移仍 preexisting；new/fixed 配对；truncated 抑制 fixed；空 code → approximate。

### 偏离规格

1. MCP `baseline` 参数未下传到 server；server 固定 `auto`。要支持 `none`/`last_success`/`task_id` 需扩展 `TaskQuery` 或独立 RPC（未做，避免非 additive 行为默契变更过大）。
2. fuzzy rename 只抬 `approximate`，不单独输出「疑似移动」文案行。

---

## PR5 — 机制五：流式生命周期 + Cancel

### 改动文件

| 文件 | 变更 |
|------|------|
| `crates/rc-core/src/progress.rs` | **新建**：Compiling/Checking 解析；参考文案（非 ETA） |
| `crates/rc-worker/src/docker.rs` | 流式行解析 + watch 通道 |
| `crates/rc-worker/src/runner.rs` | ≥2s 节流上报；`units_seen_total` 落结果 |
| `crates/rc-server/src/app.rs` | 内存 `progress_snapshot`；history 参考字段 |
| `crates/rc-server/src/store.rs` | `units_seen_total` 迁移列；`history_ref` |
| `crates/rc-server/src/schema.sql` | additive 列 |
| `crates/rc-server/src/grpc_worker.rs` | 进度不写 task_events 的 unit 字段 |
| `crates/rc-server/src/grpc_agent.rs` | `return_on_progress` + `progress_version`；`CancelTask`（server 写 CANCELED） |
| `crates/rc-agent/src/client.rs` / `mcp.rs` / `engine.rs` | cancel 工具；进度渲染 |

### 关键决策

- Cancel 镜像 admin：先 terminal + verdict CANCELED，再 `CancelTaskId`。
- classify 永不产出 CANCELED。

### 测试

- progress 解析单测；MCP tools 含 cancel（9 工具）。

### 偏离规格

1. **Agent 轮询尚未默认打开 `return_on_progress`**（字段已就绪，旧行为保留）；可在后续把 `get_result` 改为游标闭环以降低无意义轮询。
2. **worker emitter 在 `run_build` 返回后 drop**，可能丢掉最后一次 progress（终态结果仍完整）。

---

## 五个 kill-switch（§6）

| 开关 | agent (`AgentConfig`) | server (`Policy`) |
|------|----------------------|-------------------|
| `verdict_v2` | 有（渲染侧可退回旧 kind；worker 仍写 verdict） | 有 |
| `task_contract_env` | 有（关则不注入 contract env / 不自动补救） | 有 |
| `budget_gate` | 有 | 有 |
| `diag_delta` | 有 | 有（控制读时计算） |
| `unit_progress` | 有（渲染） | 有 |

**偏离**：server 侧开关未全部贯穿 worker 路径（例如关 `unit_progress` 时 worker 仍可能上报）；回滚主要靠 agent 侧与 server 读路径。

---

## 已知未尽事项

1. Server admit 路径对请求 env 的权威 `resolve_env` 重算（PR2 偏离 1）。
2. `baseline` 请求模式端到端（PR4 偏离 1）。
3. `format_result` 严格分槽 assemble（PR3 偏离 1）。
4. Agent `get_result` 默认启用 progress 游标（PR5 偏离 1）。
5. 设计文档 §8 后续项（追查率埋点、项目级 baseline 文件、ETA 分桶）本期不做。
6. 既有 egress 未提交改动保持不动；其通知已纳入 Notice 状态机（`egress_pending`/`egress_refused`）。

---

## 汇总：规格符合度

| 机制 | 核心交付 | 自验 |
|------|----------|------|
| 一 Verdict | 规则表 + 证据 + 映射 + 缓存过滤 + 渲染 | 全绿 |
| 二 TaskContract | trait + env 闭环 + ABI3 + 自动补救 | 全绿 |
| 三 Budget/Notice | 预算门 + 状态机迁移 | 全绿 |
| 四 DiagDelta | strict_key + baseline auto + 渲染 | 全绿 |
| 五 Lifecycle | units_seen + history_ref + cancel + progress 字段 | 全绿 |
| Kill-switch | 五开关 | 配置就绪 |
