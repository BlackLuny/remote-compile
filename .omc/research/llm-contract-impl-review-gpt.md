# LLM 契约机制实现评审（GPT）

评审范围：`grok-only.diff` 中的本次实现；对照 `docs/proposals/llm-contract-mechanisms.md` v2（§1–§7、§9）及 `.omc/research/llm-contract-impl-report.md`。未把已分离的 egress/proxy/multiroot/excludes 既有改动本身算作本次发现。以下行号均为仓库现行文件。

## 结论

本轮不能按当前状态合入。共有 3 个 BLOCKER（其中 1 个是实现者已自列偏离的风险裁定，不重复计为新发现）：

1. server 仍然直接信任 agent 提供的 `ResolvedProfile.canonical`，没有从实际执行字段和请求 env 权威重建 effective profile；可形成“执行 B、缓存键 A”的缓存投毒。
2. `TaskContract::parse_outcome` 没有接入 worker，导致成功的默认 `test` 不返回 `TestSummary`，而覆盖命令/非 test 命令反而仍可被 libtest 文本误判为 CODE。
3. BudgetGate 没有覆盖所有 MCP 文本出口，且已接入的路径也能超过 8KB；I3 尚未成立。

此外，自动 baseline 当前总选中任务自身而完全不产 delta，异步 OOM 不会触发自动补救，进度仍逐条写 `task_events`，cancel 缺少归属校验且存在终态覆盖竞态。这些均需本轮修复。

## 发现

### F1 — BLOCKER — TaskContract outcome 路径是死代码：成功 test 丢摘要，覆盖命令仍会被误解析

位置：`crates/rc-core/src/diag.rs:475-489,572-605,791-817`，`crates/rc-core/src/contract.rs:45-51,175-200`，`crates/rc-worker/src/runner.rs:256-305`

论据：

- worker 只调用 `classify_with_exec`，从未调用 `TaskContract::parse_outcome`，也没有传递“命令是否为契约默认”的事实。
- `classify_with_exec` 对所有 task/command 无条件解析 libtest；规则 4 本身也不检查 `task_type`。因此 `task=test` 的 command/profile override、甚至 build/custom 日志里偶然出现 libtest 摘要，都可能产出 `ATTR_CODE`，违反 §2.2/F24。
- 相反，成功默认 test 先在规则 0 返回，`Classification.test_summary` 保持 `None`，所以最核心的“✓ 测试通过：N passed...”根本不会出现。
- contract 单测只直接测试了未接线的 trait，不能证明生产路径行为。

建议修复：

- 在 resolution/assignment 中携带明确的 parser/`command_is_default` 语义，worker 通过对应 TaskContract 解析 outcome；
- 规则 4 只接受已由启用的 TestContract 产出的 `TestSummary`；
- success 分支也保留 summary；
- 增加 worker/Engine 级测试：默认 test 成功带计数；command override/profile task override 不解析；非 test 日志含摘要不产 CODE。

### F2 — BLOCKER — BudgetGate 未覆盖所有 MCP 出口，现有门也不是硬 8KB 上限

位置：`crates/rc-agent/src/mcp.rs:101-121,152-172,175-235,246-383`，`crates/rc-core/src/budget.rs:177-193,250-258`

论据：

- 只有 `check` 调用 `gate_response`；`get_result`、build profile、env、worker、cancel 以及 Tool error 文本都绕过统一出口。§3/I3 要求“所有 MCP 响应文本必经”。
- `gate_response` 先截到 8192B，随后再追加省略标记，必然可能超过总预算；测试还把 `RESPONSE_BUDGET + 64` 当作合格。
- `get_log` 先生成最多 8192B 的 gated body，再额外加 header 和分页提示，同样超过总预算。

建议修复：

- 在 `call_tool`/最终 text-content 构造处设置唯一预算出口，所有成功及 Tool error 文本统一经过；
- 预算计算必须预留 header/marker/pagination 的字节，最终返回值断言 `<= 8192`；
- 加逐工具契约测试，而不是只测 BudgetGate helper。

### F3 — MAJOR — auto baseline 总会先选中当前任务自身，diag delta 实际不产生

位置：`crates/rc-server/src/store.rs:762-792`，`crates/rc-server/src/app.rs:877-909`

论据：

- `task_status` 在当前任务已落库后调用 `resolve_baseline(..., "auto")`。
- 查询按 `finished_at DESC LIMIT 1`，没有排除当前 task id，因此最近完成任务就是当前任务。
- app 发现 `base.id == t.id` 后直接跳过计算；于是正常读取终态结果时 `diag_delta` 为空。`last_success` 对成功的当前任务也有同样问题。
- 现有 delta 单元测试只覆盖集合算法，没有覆盖“完成两个同 worktree 任务后读取第二个”的真实选择路径。

建议修复：

- baseline 查询显式接收并排除 current task id，或限定 `finished_at < current.finished_at` 并用 id 作稳定 tie-break；
- 增加 store+app 集成测试，覆盖 auto、last_success、首次任务、同毫秒完成以及不跨 worktree。

### F4 — MAJOR — 自动补救只在首次 `check` 的短同步等待内生效，典型异步 OOM 永不重试

位置：`crates/rc-agent/src/engine.rs:372-425,431-534,584-588`

论据：

- `maybe_remediate` 只从 `check` 中、且仅当短等待已经拿到 terminal result 时调用。
- 默认等待仅数秒；OOM 通常发生于较长编译。此时 `check` 返回 task id，之后 `get_result` 只 render，白名单规则再明确也不会触发补救。
- 第二次重试若 30 秒后仍未完成，后续 `get_result` 也不知道第一次 verdict，无法实现“双败以首次为主”。
- 双败同步路径还用第二次的 `status.task_id` 格式化第一次结果（`engine.rs:515-525`），把首次证据标到错误 task id。

建议修复：

- 把补救状态建模为按原始请求/task 持久或至少进程内可恢复的一次性状态机，并在任何获取到首次 terminal verdict 的路径触发；
- 明确记录 first_task_id/first_result/retry_task_id；
- retry pending 时返回关联信息，后续轮询能合并双结果；加异步 OOM 和双败测试。

### F5 — MAJOR — rustc_crash 的 SIGSEGV 条件缺少编译上下文，测试进程崩溃会被误判 INFRA

位置：`crates/rc-core/src/diag.rs:657-685,1216-1237`

论据：

- 规格规则 7 要求 SIGSEGV/SIGABRT 且来自 `error: could not compile` 上下文。
- 实现对 SIGSEGV 无条件命中，只有 SIGABRT 检查 compile context。
- 这会把“test binary SIGSEGV、无摘要”从规则 10 UNKNOWN 改成 INFRA，违背 precision-first 与 §1.7 的崩溃 fixture 意图。
- 测试注释甚至把当前不对称行为固化为“SIGSEGV always”，没有断言规格条件。

建议修复：两个 signal 都要求精确的 cargo compile-failure 上下文，并增加 test-binary SIGSEGV/SIGABRT 无摘要均为 UNKNOWN 的 fixture。

### F6 — MAJOR — env raw marker 扫描全日志，而不是限定最后 200 行

位置：`crates/rc-core/src/diag.rs:345-357,711-734,805-817`

论据：

- `Facts` 注释承诺 full log 只供 env_hints，分类规则使用 `log_tail`。
- 规则 9 实际调用 `looks_like_env_error(facts.raw_output)`，会被数千行以前已恢复的 `pkg-config`/download 等文本命中，把真正未知的最终失败归成 PROJECT_CONFIG。
- 这违反 §1.5 “规则 6/7/8/9 只作用于最后 200 行”。

建议修复：规则 9 只检查 `log_joined`/tail，并从命中的 tail 行生成证据；full raw 仅保留给 env_hints。

### F7 — MAJOR — unit progress 仍逐条写 task_events

位置：`crates/rc-worker/src/runner.rs:223-249`，`crates/rc-server/src/grpc_worker.rs:248-260`

论据：

- emitter 为每次 unit update 构造 `phase="building"` 的 `TaskProgress`。
- server 对所有非空 phase 都执行 `add_timeline` 和 `set_status`，所以每个节流后的 crate update 仍落 SQLite `task_events`；注释所称“unit progress memory-only”与实际数据形状矛盾。
- 大 workspace 长任务仍会造成规格 F14.4/F26.3 要避免的行膨胀和单写锁压力。

建议修复：unit update 的 phase 置空或拆分独立消息；server 只对真正的 phase transition 写 timeline，并加数据库行数断言。

### F8 — MAJOR — cancel 没有调用方归属校验，任何有效 agent token 都可取消任意任务

位置：`crates/rc-agent/src/client.rs:152-158`，`crates/rc-server/src/grpc_agent.rs:29-39,566-584`

论据：

- agent 发出的 `CancelTaskReq.agent_session` 固定为空。
- server 只校验 fleet-wide token 有效，取到 task 后不比较 project_id、agent_session 或 subscriber。
- 规格 §5.5 明确要求 task 的 project_id 与调用方项目身份一致；当前任务 id 一旦泄露即可跨项目取消。

建议修复：让认证上下文绑定 project/session，或请求携带可由 server 校验的 project identity；至少校验 task owner/subscriber，不能信任一个未经绑定的明文 project_id。

### F9 — MAJOR — cancel 与 TaskDone 的检查/写入非原子，CANCELED 可被覆盖或覆盖已完成结果

位置：`crates/rc-server/src/grpc_agent.rs:572-610`，`crates/rc-server/src/app.rs:651-710`，`crates/rc-server/src/store.rs:692-722`

论据：

- cancel 先读非终态，再无条件 `UPDATE tasks`；TaskDone 也先读非终态，随后无条件 complete。
- 两条路径交错时，cancel 可在真实完成后把结果覆盖成 canceled，或 TaskDone 在 cancel 后把 CANCELED 覆盖成 done/failed。
- `on_task_done` 的“terminal 丢弃”只是一瞬时读取，不是 compare-and-set，不能保证 §5.5。

建议修复：提供事务内条件更新（`WHERE status NOT IN terminal...`）并检查 affected rows；cancel 成功写入后 TaskDone 的条件完成必须失败并丢弃。增加双向竞态测试。

### F10 — MAJOR — baseline-off 被当作可静默的 Warning，Critical 契约没有满足

位置：`crates/rc-agent/src/engine.rs:1103-1115,1174-1181`，`crates/rc-agent/src/scanner.rs:174-189`

论据：

- scanner 的 baseline-off 文本进入统一 `"scanner"` category，并被标为 Warning；重复 identity 后会静默。
- 规格 §3.2 明列 baseline-off 是影响结果解释的 Critical，必须每次 compact。
- 虽然 exclude 另有 Critical，但其 compact 只有 `exclude: ...`，没有表达 baseline 已关闭，不能替代该正确性信息。
- 多条 scanner warning 还共享同一 category；状态机在同一快照内反复覆盖同一个 key，多个 warning 的重复快照无法稳定静默/复发。

建议修复：把 baseline-off 做成独立 Critical category 与自包含 compact；同 category 的结构化字段先聚合为单个 Notice。

### F11 — MINOR — normalize_spans 的“裸 span”不是尾缀匹配，并且空 code 的 approximate 判定漏 warning

位置：`crates/rc-core/src/delta.rs:8-17,77-103`

论据：

- 规格要求裸 `:\d+:\d+` 尾缀；实现 regex 没有 `$`，会删除消息中部任何相同形状的语义片段。
- approximate 条件要求任一诊断 code 为空；实现只检查 `level == "error"`，空 code warning 不会标 approximate。

建议修复：将 bare regex 锚定到允许的尾部位置；empty-code 检查覆盖所有参与 delta 的诊断，并补语义数字/中部冒号数字/空 code warning 测试。

### F12 — MINOR — progress snapshot 在终态后永不清理

位置：`crates/rc-server/src/app.rs:39-52,115-125,877-925`

论据：

- `progress` map 只 insert/read，没有在 terminal、cancel、supersede 或 retry 时 remove，常驻 server 会按历史任务数增长。

建议修复：在 done/failed/canceled/superseded 及重新排队时清理或重置 snapshot，并加长生命周期任务数测试。

### F13 — MINOR — 关键契约测试停留在 helper/死代码层，实施报告的“全绿”不能证明规格验收

位置：`crates/rc-core/src/budget.rs:230-258`，`crates/rc-core/src/contract.rs:380-439`，`crates/rc-server/src/store.rs:2457-2489`

论据：

- Budget 测试允许超过总上限，raw 测试只断言产生 next offset，没有进行第二次续读。
- Contract parser 测试直接调用未接生产路径的 trait。
- 没有覆盖 server canonical/env 重建到 worker 所见 env、第二次完成任务的 auto delta、cancel/TaskDone 竞态、所有 MCP tool 文本上限、unit progress 不增 task_events。
- SQLite 新列的 v0 升级路径有测试，迁移本身未发现阻断问题；但这不能代替上述端到端契约测试。

建议修复：按 §1.7/§7 验收点增加跨 crate 集成测试，优先把本评审 F1–F9 各自变成可复现回归。

## 实现者自列偏离的风险裁定

以下只做风险裁定，不重复计为上述发现：

| 自列偏离 | 裁定 | 理由 |
|---|---|---|
| PR2：server 未再次 `resolve_env` | **需本轮修复（BLOCKER）** | `app.rs:202-235` 直接把 wire profile 交给 `compute_for`；`fingerprint.rs:137-149` 只 hash 未校验的 `profile.canonical`，而 worker 在 `runner.rs:353-401` 执行实际 `profile.env`/assignment command。请求可令 canonical=A、执行字段=B，形成“执行 B、缓存键 A”；server 必须用唯一 resolver/canonicalizer 从实际字段和请求 env 重建，并加不一致请求的端到端测试。 |
| PR2/PR4：MCP baseline 未传到 server、固定 auto | **需本轮修复（MAJOR）** | 工具 schema 对外承诺 `none/last_success/task_id`，但所有值静默当 auto；这是错误 API 契约，不宜带着假参数上线。 |
| PR3：format_result 未严格 SlotPiece assemble | **需本轮修复（MAJOR）** | headline 位于前部不等于槽优先级；通知/诊断预算和 Critical 保留均无机械保证，且总上限本身已破，见 F2。 |
| PR3：raw 行内续读只在 agent helper，server 未实现 | **需本轮修复（MAJOR）** | MCP schema 不接受 `line_byte_offset`，agent 也不传给 `LogQuery`，server 忽略该字段；调用提示给出一个无法前进的请求，会永远重复首段。 |
| PR4：fuzzy rename 只置 approximate、不显示“疑似移动” | **可接受留档** | 不影响 new/fixed 的确定集合，且 approximate 避免过度断言；属于信息召回不足，可后续补文案。 |
| PR5：agent 默认轮询未启用 `return_on_progress` | **可接受留档，但应修正文档表述** | proto 默认 false 符合向后兼容；即时 `get_result` 仍可读取 snapshot。它降低长轮询效率，但不单独破坏结果正确性。 |
| PR5：emitter 可能丢最后一次 progress | **需本轮修复（MINOR）** | `runner.rs:229-249` 在 2 秒窗口内收到 change 后直接 `continue`，没有 timer 在窗口结束时发送 latest；若之后无新 change，整窗最后值永久丢失。应采用 interval/debounce 并在 channel 关闭前 flush。 |
| 五 kill-switch 未全部贯穿 server/worker | **需本轮修复（MAJOR）** | §6 要求机制独立回滚；若关 `unit_progress` 仍写入事件、关 contract/env 仍接受不一致字段，开关不能作为安全回滚手段。 |

## 必须本轮修复 / 可接受留档

必须本轮修复：

- F1–F2 两个新发现 BLOCKER，以及自列 PR2 env/server 偏离的 BLOCKER；
- F3–F10 全部 MAJOR；
- 自列偏离中 baseline 端到端、raw 续读、严格预算槽、kill-switch 贯穿；
- 自列 emitter 丢最后进度的偏离；
- 至少补齐能复现上述问题的契约级测试。

可接受留档：

- fuzzy rename 暂不输出单独提示；
- agent 暂不默认打开 `return_on_progress`（前提是对外不宣称已经交付游标闭环优化）；
- F11–F13 可按 MINOR 排在 BLOCKER/MAJOR 之后，但建议仍随本轮收口，改动局部且能防止后续语义漂移/内存增长。

未发现需要阻断的 SQLite `units_seen_total` 旧库迁移问题；`ResultCache.put` 的 `{success, compile_error}` 过滤、RESOURCE→env_error、磁盘满→INFRA、timeout 优先级、compile error 不受 raw marker 改判、SIGKILL cargo 精确串、truncated 不报 fixed、baseline 查询键包含 worktree、classify 不产 CANCELED 等点在静态检查中符合规格。由于任务要求只写本评审文件，本次未运行会写入 `target` 的 cargo 测试；“全绿”结论仅来自实施报告，不作为上述语义问题已被验证的证据。
