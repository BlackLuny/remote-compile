# Intent / Query Surface 实现检视

## 总判

**REJECT**

当前实现已经搭起主要字段和大部分 happy path，但 S1 的 wire/worker/Receipt 闭环仍未成立：
worker 会二次推断 `command_is_default`，普通远端执行产生的 `EffectivePlan` 丢失
`PathContext`，server 完成任务时又直接持久化该不完整结果。结果是实际 package scoped
命令虽然可能正确执行，最终 Receipt 却只能显示 `scope=unknown`，headline 也无法标出
`scope=package`。此外 delta 基线没有按提案定义的 `scope_hash` 隔离。

检视基准：`docs/proposals/intent-and-query-surface.md`，重点为 §3.3–§3.8、
§4.3–§4.7、§5.2、§6.2、§7.2、§9.1。

## 12 点 CLOSED / OPEN 表

| # | 检视点 | 状态 | 结论与证据 |
|---:|---|---|---|
| 1 | path → package | OPEN | 普通 workspace member 的最长前缀匹配已实现，并能生成 package scope（`crates/rc-core/src/scope.rs:80-115`）；但 root package 的成员发现/空目录匹配不完整，见发现 F3。 |
| 2 | workspace glob + exclude | OPEN | `exclude` 只应用于 glob、显式 member 不过滤的主规则已实现（`crates/rc-core/src/scope.rs:288-299`）；但 glob 展开器只真正支持尾部 `/*`（`crates/rc-core/src/scope.rs:333-365`），不满足一般 workspace glob，见 F3。 |
| 3 | 唯一 `resolve_command` + worker `command_is_default` | OPEN | 优先级“显式命令 > profile task > adapter default”已集中到 `resolve_command`（`crates/rc-core/src/scope.rs:138-205`），但 scope mismatch 没有 Critical Notice，worker 还会重新比对命令并 OR 权威值（`crates/rc-worker/src/runner.rs:284-291`），见 F1/F4。 |
| 4 | agent 提交 PathContext + server 权威 resolve | CLOSED | agent 提交 `path_context`（`crates/rc-agent/src/engine.rs:340-355`），server 消费它并用同一 `resolve_command` 重算（`crates/rc-server/src/app.rs:271-308`），最终 command 进入 server 重建的 profile/fingerprint。缺少 agent/server 布尔不一致指标，记为 F9，不阻断本小项。 |
| 5 | supersede + delta scope 隔离 | OPEN | supersede key 已包含 `scope_hash`（`crates/rc-server/src/app.rs:461-481`）；但 auto/last_success baseline 仅比较 command 文本（`crates/rc-server/src/store.rs:802-831`），没有比较 `scope_hash`，见 F2。 |
| 6 | server `task_meta` → assignment | OPEN | server 能从 `task_meta` 下发 `command_is_default/scope_hash`（`crates/rc-server/src/app.rs:672-704`）；但写入错误被丢弃，且没有保存/下发 PathContext，导致 Receipt 数据链断裂，见 F5/F6。 |
| 7 | `get_log` honest empty state | CLOSED | 无日志返回 `empty_reason=no_log`；grep 零命中保留过滤前 `total_lines`、返回 `matched_lines=0` 和 `no_match`（`crates/rc-server/src/app.rs:992-1039`）；agent 也区分两种文案（`crates/rc-agent/src/mcp.rs:215-228`）。 |
| 8 | publish 不冻结默认 `-p` | CLOSED | 仅 `command_is_default=false` 才写入 `profile.tasks`（`crates/rc-agent/src/engine.rs:701-718`），默认 package scoped command 不会被冻结。 |
| 9 | `format_result` Receipt + diagnostics footer | OPEN | footer 已优先指向 `get_diagnostics`（`crates/rc-agent/src/engine.rs:1846-1857`），Receipt 渲染也读取 `effective_plan.path`（`crates/rc-agent/src/engine.rs:1623-1634,1690-1715`）；但普通 worker 结果的 path 为 `None`，所以 package headline/Receipt 实际无法兑现，见 F5。 |
| 10 | MCP `get_diagnostics` | OPEN | 工具、过滤、only_new、分页和 hard max 100 均已实现（`crates/rc-agent/src/mcp.rs:322-349,544-560`；`crates/rc-agent/src/engine.rs:741-859`）；但 next 导航丢失 `code/file_prefix/only_new/baseline`，翻页会改变查询集合，见 F7。 |
| 11 | `resolve_image` | OPEN | exact id/ref/digest、歧义返回 candidates、not-found 引导均存在；但实现额外接受任意 substring/ends-with，可能把规范未允许的引用静默解析成唯一镜像（`crates/rc-server/src/images.rs:77-123`），见 F8。 |
| 12 | gRPC wait 上限 120s | CLOSED | `wait_secs.min(120)` 已落地（`crates/rc-server/src/grpc_agent.rs:270-279`），与提案一致。 |

汇总：**4 CLOSED / 8 OPEN**。

## 发现清单

### F1 — BLOCKER — worker 仍会重算 `command_is_default`

证据：`crates/rc-worker/src/runner.rs:284-291`：

```rust
let command_is_default = assignment.command_is_default
    || rc_core::contract::command_is_default_resolved(...);
```

提案 §3.4 明确要求 worker 只读 server 权威布尔。当前 OR 回退会把 server 明确下发的
`false` 翻成 `true`：例如显式 command 恰好与默认命令文本相同。这样 TestSummary/parser
门控与命令来源不再一致。proto3 普通 `bool` 不能区分“旧 worker assignment 未携带字段”
与“新 server 权威下发 false”，因此不能用 OR 兼容。

### F2 — MAJOR — delta baseline 用 command 文本代替 `scope_hash`

证据：`crates/rc-server/src/store.rs:802-831` 使用 `AND command = ?6`。

提案 §3.7 的 scope identity 包含 scope kind、packages、`command_is_default` 和 command。
两个任务可能命令文本相同但来源不同，例如默认 workspace command 与同文本的显式
override，或 package default 与同文本的 profile override。当前实现会跨这两类 scope
计算 new/fixed，违反“无同 scope 历史视同 none”。

### F3 — MAJOR — Cargo workspace member 解析不是 Cargo 等价 glob

证据：

- `crates/rc-core/src/scope.rs:333-365` 只有 `strip_suffix("/*")` 分支；
  注释声称支持 `crates/*/foo`、`/**`，代码实际对仍含 `*` 的其它形式不产出任何 member。
- `crates/rc-core/src/scope.rs:277-323` 在 `[workspace].members` 非空时没有把同时存在的
  root `[package]` 加入成员清单；提案特意说明“virtual workspace 无 root package 时仅
  members”，反向意味着非 virtual workspace 的 root package 必须参与解析。
- root package 被表示为 `dir=""`（`crates/rc-core/src/scope.rs:277-283`），但前缀判断
  `relative == m.dir || relative.starts_with(m.dir + "/")`
  （`crates/rc-core/src/scope.rs:80-89`）无法匹配非空的 `src/...`。

影响：合法 workspace 布局会静默回退全 workspace，重新引入 silent scope expansion。

### F4 — MAJOR — profile task 与 path 不一致时没有 Critical Notice

证据：全仓 `scope_mismatch` 仅出现在
`crates/rc-core/src/scope.rs:172-180` 的 `resolve_note` 文本；没有 Notice 创建、Critical
severity 或 identity 去重实现。

提案 §3.3 要求 profile override 与派生 package 不一致时产生 Critical
`scope_mismatch` Notice，而不是只把提示埋入一个目前也无法到达终态 Receipt 的字段。

### F5 — BLOCKER — 普通终态结果丢失 PathContext，Receipt 不能闭环

证据：

- `TaskAssignment` 只携带 `command_is_default` 和 `scope_hash`，没有 PathContext
  （`crates/rc-core/proto/rc.proto:551-568`）。
- worker 创建 `EffectivePlan` 时明确写 `path: None`
  （`crates/rc-worker/src/runner.rs:338-351`）。
- server 收到完成事件后直接把 worker result 传给 `complete_task`
  （`crates/rc-server/src/app.rs:847-878`），没有用提交时的权威解析结果补齐 plan。
- `format_result` 依赖 `effective_plan.path` 才能生成 package headline 和 scope
  （`crates/rc-agent/src/engine.rs:1623-1664,1690-1699`）。

因此普通执行会输出 `scope=unknown`，且 headline 缺失 `scope=package`，不符合 §5.2
“每次终态（含 cache hit）必显”的要求。cache-hit 分支也只在整个
`effective_plan.is_none()` 时补 plan（`crates/rc-server/src/app.rs:395-404`）；
若缓存结果已有 `Some(plan)` 但 `path=None`，仍不会修复，而且补过的 result 没有回写
cache-hit task row，后续轮询仍可能取到未补版本。

### F6 — MAJOR — `task_meta` 是 best-effort，写失败会静默降级

证据：`crates/rc-server/src/app.rs:427-431` 使用
`let _ = self.store.set_setting(...)` 丢弃错误；dispatch 缺 metadata 时回退字符串重算并
清空 `scope_hash`（`crates/rc-server/src/app.rs:672-688`）。

这会让新任务在内部存储异常时静默违反 server 权威 contract，同时跨 scope supersede
key 已写入 row、assignment scope_hash 却为空，两个消费者看到不同身份。关键 task
metadata 应随 task 原子持久化，至少不能忽略写入错误。

### F7 — MAJOR — `get_diagnostics` 的 next 链接不保持过滤条件

证据：`crates/rc-agent/src/engine.rs:854-857` 的 next 只带
`task_id/severity/offset/limit`，没有带 `code`、`file_prefix`、`only_new`、`baseline`。

例如第一页查询 `code=E0063`，照 next 请求第二页会变成“所有 error”的 offset，出现
重复、跳项或错误总数，破坏稳定分页语义。

### F8 — MAJOR — `resolve_image` 接受规范外 substring，可能 silent pick

证据：`crates/rc-server/src/images.rs:88-111` 接受 `id.ends_with`、`id.contains`、
`image_ref.contains`，并在命中唯一时直接返回。

提案 §6.2 允许 exact id、完整 ref、repo+tag、唯一 short prefix；不允许任意 substring。
如 `"rust"` 只命中一个 image_ref 时会被静默选中，而不是 not_found。多命中时的
`short_hits` 仍使用 `contains`，也不是“唯一 prefix”。

### F9 — MINOR — server 没有记录 agent/server 预计算不一致指标

证据：agent 提交 `command_is_default`（`crates/rc-agent/src/engine.rs:353-354`），server
权威重算后没有比较 `req.command_is_default`，只对 fingerprint mismatch 写 warning
（`crates/rc-server/src/app.rs:300-308,326-334`）。

提案 §3.4 要求交叉校验并记录 metrics，用于 `agent_server_fp_match_rate` 回归哨。

## 修复优先级

1. **P0：修复 F1、F5。** 移除新协议下 worker 的字符串重算；让 server 权威
   `ResolvedCommand/PathContext/EffectivePlan` 随 task 持久化并下发，或在完成时由
   server 可靠补齐，再增加真实远端完成与 cache replay 的 Receipt 端到端测试。
2. **P0：修复 F2。** 将 `scope_hash` 作为 task 的正式持久字段，并让 auto /
   last_success baseline 以它过滤；无同 scope 历史返回 none。补“同 command、不同
   command_is_default/scope kind 不互作 baseline”测试。
3. **P1：修复 F3、F4。** 使用与 Cargo 语义一致的 glob 匹配/展开，纳入非 virtual
   workspace 的 root package；补 root package、`crates/*/foo`、`**`、exclude、自校验
   用例，并把 scope mismatch 接入 Critical Notice 状态机。
4. **P1：修复 F6、F7。** task metadata 不得 best-effort；diagnostics next 必须完整
   保留查询条件，并增加多页过滤回归测试。
5. **P2：修复 F8、F9。** 镜像引用只接受定义过的 exact/prefix 形式；补歧义与
   substring not-found 测试；增加 agent/server resolution mismatch 指标。

## 验证记录

- `cargo test -p rc-core scope --lib`：8 passed。
- `cargo test -p rc-server`：129 passed。
- `cargo test -p rc-agent`：127 passed。
- `cargo test -p rc-worker`：79 passed。
- 合计 343 passed；现有测试未覆盖上述关键反例。
- 本次仅新增本报告，未修改业务代码。
