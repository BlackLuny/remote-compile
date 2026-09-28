# LLM Contract 第二轮修复闭环核验

核验范围仅限修复清单 R1–R11、实现报告的「第二轮修复」声明，以及上轮 F1–F10 的闭环状态；未重新展开全面评审。按“只写本文件”的约束，本轮未运行会生成 `target/` 等文件的构建或测试命令，结论来自现行代码和已有测试代码的静态核验。

## 逐项结论

### R1 — 未闭合

默认测试命令的可观测行为已补齐：worker 计算 `command_is_default_resolved` 并调用 `classify_with_exec`（`crates/rc-worker/src/runner.rs:270-288`）；Rule 0 会携带测试汇总（`crates/rc-core/src/diag.rs:475-511`），Rule 4 也受默认测试命令门控（`crates/rc-core/src/diag.rs:594-625`）。

但清单要求的生产接线没有完成。`classify_with_exec` 直接调用 `parse_test_summary`（`crates/rc-core/src/diag.rs:817-849`），没有通过 `TaskContract::parse_outcome`；trait 方法及 Test 实现仍只存在于 `crates/rc-core/src/contract.rs:45-51`、`crates/rc-core/src/contract.rs:307-332`，生产路径无调用。因此上轮 F1 只修复了外部效果，没有关闭“统一走 TaskContract parser”的契约要求。

### R2 — 已闭合

server 从请求的结构化 profile 重新计算有效 profile：命令解析和 `effective_profile` 调用位于 `crates/rc-server/src/app.rs:226-259`，最终持久化的是 server 计算出的 profile（`crates/rc-server/src/app.rs:385-400`）。`effective_profile` 会重新解析 env 并重建 canonical/fingerprint，不信任客户端 canonical 字符串（`crates/rc-core/src/contract.rs:128-208`）。覆盖测试见 `crates/rc-core/src/contract.rs:588-625` 和 `crates/rc-server/src/app.rs:1260-1287`。

### R3 — 未闭合

所有 MCP tool 成功文本和 tool error 文本已经进入唯一的 `gate_response`：`crates/rc-agent/src/mcp.rs:109-136`、`crates/rc-agent/src/mcp.rs:71-90`；硬上限实现见 `crates/rc-core/src/budget.rs:264-281`。raw continuation 也有真实 schema、参数处理和切片实现（`crates/rc-agent/src/mcp.rs:189-260`、`crates/rc-agent/src/mcp.rs:497-510`、`crates/rc-core/src/budget.rs:175-262`）。

但 Critical slot 保留没有接入实际返回路径。`assemble_result_with_notices` 虽在 `crates/rc-core/src/budget.rs:283-301` 定义，却没有生产调用；当前 `with_note` 只是把 notice 追加到正文尾部（`crates/rc-agent/src/engine.rs:1387-1402`），随后由 `gate_response` 对整段文本截断，因此尾部 Critical notice 仍可能丢失。上轮 F2 未真正关闭。

### R4 — 已闭合

baseline 查询字段沿 agent、gRPC、app 全链传递（`crates/rc-agent/src/client.rs:130-150`、`crates/rc-agent/src/mcp.rs:178-186`、`crates/rc-server/src/grpc_agent.rs:274-345`、`crates/rc-server/src/app.rs:920-971`）。SQL 同时排除当前 task，并使用完成时间和 id tie-break 只选择严格更早任务（`crates/rc-server/src/store.rs:774-818`）。同 worktree 第二任务和首任务无 baseline 的覆盖见 `crates/rc-server/src/app.rs:1197-1257`。上轮 F3 已关闭。

### R5 — 未闭合

同步结论和异步 `get_result` 已共享 remediation handler，状态以 task id 保存；双任务失败时也使用首轮/重试的正确 task id（`crates/rc-agent/src/engine.rs:373-382`、`crates/rc-agent/src/engine.rs:444-456`、`crates/rc-agent/src/engine.rs:492-696`、`crates/rc-agent/src/engine.rs:748-770`）。`CARGO_BUILD_JOBS=2` 和文档也已更新（`crates/rc-core/src/contract.rs:64-75`、`docs/proposals/llm-contract-mechanisms.md:262-264`）。

但 no-op guard 比较的不是完整有效环境。slot 注册时只保存 `req.env.clone()`（`crates/rc-agent/src/engine.rs:373-380`），handler 也仅拿补丁与该请求 env 比较（`crates/rc-agent/src/engine.rs:585-603`）。若 profile env 已含同值而请求 env 未显式携带，实际有效环境不会变化，代码仍会发起强制重试。随后对 profile 的补丁（`crates/rc-agent/src/engine.rs:605-616`）不能补救重试前判断。现有测试没有覆盖这一 profile-env 场景。因此上轮 F4 未关闭。

### R6 — 未闭合；存在新的终态复活路径

cancel 已携带并校验 project id（`crates/rc-core/proto/rc.proto:307-313`、`crates/rc-agent/src/client.rs:162-169`、`crates/rc-server/src/grpc_agent.rs:575-603`）；`complete_task` 使用 compare-and-set，`set_status` 也禁止覆盖终态（`crates/rc-server/src/store.rs:628-638`、`crates/rc-server/src/store.rs:695-729`）。因此上轮 F8 已关闭。

然而 `Store::requeue` 仍是只按 id 的无条件 `UPDATE ... status='queued'`（`crates/rc-server/src/store.rs:661-673`）。`on_task_done` 先读一次状态，之后在缺 blob 或可重试 infra 分支调用 requeue（`crates/rc-server/src/app.rs:683-718`）；cancel 若在首次读取后、requeue 前胜出，终态 `canceled` 会被重新写成 `queued`。现有竞态测试只覆盖 terminal completion 的 CAS（`crates/rc-server/src/store.rs:2800-2843`），未覆盖该 TaskDone/requeue 竞态。所以上轮 F9 仍未关闭，并暴露了清单声称“双向竞态均终态不回滚”之外的新问题。

### R7 — 未闭合

unit phase 已保持空值，server 仅以内存进度覆盖，并有 kill-switch（`crates/rc-worker/src/runner.rs:229-260`、`crates/rc-server/src/grpc_worker.rs:248-269`）；正常结束、失败、取消和终态查询均有 progress map 清理（`crates/rc-server/src/app.rs:730-740`、`crates/rc-server/src/app.rs:788-795`、`crates/rc-server/src/grpc_agent.rs:637-637`、`crates/rc-server/src/app.rs:923-929`）。因此上轮 F7 的“不要按 heartbeat 写 DB”部分已关闭。

但 emitter 没有在 throttle 窗口到期时主动 flush。窗口内收到变更会直接 `continue`（`crates/rc-worker/src/runner.rs:248-250`），之后若没有新变更，只会等到下次变更或 build/channel 结束才发送；channel-close flush 及 join 等待虽已实现（`crates/rc-worker/src/runner.rs:233-246`、`crates/rc-worker/src/runner.rs:263-268`），仍不满足“窗口结束 flush 最新值”。

### R8 — 已闭合

SIGSEGV/SIGABRT 仅在编译上下文中判为 ICE（`crates/rc-core/src/diag.rs:680-710`）；Rule 9 只扫描截断后的 log tail（`crates/rc-core/src/diag.rs:735-759`）。测试覆盖普通 test crash 和旧标记落在 tail 之外的场景（`crates/rc-core/src/diag.rs:1250-1280`、`crates/rc-core/src/diag.rs:1318-1336`）。上轮 F5、F6 已关闭。

### R9 — 已闭合

`baseline_off` 生成自包含 Critical notice，其他 scanner warnings 统一汇入 notice 集合（`crates/rc-agent/src/engine.rs:1358-1383`）；notice 的 Critical 优先和聚合覆盖见 `crates/rc-core/src/notice.rs:171-202`。上轮 F10 的 scanner warning 聚合要求已关闭。该项不改变 R3 中“最终预算截断仍可能丢 Critical”的独立结论。

### R10 — 未闭合

regex 已改为 `OnceLock`，bare 路径尾部匹配也已收紧（`crates/rc-core/src/delta.rs:7-26`）；空 code 的 error/warning 计数逻辑见 `crates/rc-core/src/delta.rs:86-112`。

但终态 delta 没有实现“只算一次并缓存”。`task_status_with_baseline` 每次终态查询都会重新解析 history ref，并在返回对象没有 `diag_delta` 时重新计算（`crates/rc-server/src/app.rs:920-971`）；计算结果只写入本次响应中的 `result`，没有持久化或放入 server cache。下一次查询重新从 task result 读取时仍为空，因此重复执行。清单中的性能闭环未完成。

### R11 — 未闭合

同 worktree 第二任务产生 delta、首任务无 baseline 的 app/store 测试已存在（`crates/rc-server/src/app.rs:1197-1257`）。

但未找到“异步 OOM 经 `get_result` 自动 remediation 并返回最终结论”的端到端测试；remediation 代码路径也没有对应的 engine 异步状态机测试。绿色默认测试命令的覆盖目前只是 classifier 单元测试（`crates/rc-core/src/diag.rs:1282-1291`），没有贯穿 worker、server、agent/MCP 最终结论的端到端测试。实现报告也明确承认未增加真实 async OOM E2E。因此三项要求中至少第 2、3 项未闭合。

## 上轮 F1–F10 复核摘要

| 上轮项 | 本轮状态 | 对应结论 |
|---|---|---|
| F1 TaskContract 测试解析生产接线 | 未关闭 | R1 |
| F2 全局预算与 Critical 保留 | 未关闭 | R3 |
| F3 baseline 排除自身/严格更早 | 已关闭 | R4 |
| F4 remediation 状态与有效 env no-op | 未关闭 | R5 |
| F5 signal 编译上下文 | 已关闭 | R8 |
| F6 Rule 9 仅扫 tail | 已关闭 | R8 |
| F7 progress 不落 DB | 已关闭；但 emitter 闭环仍缺失 | R7 |
| F8 cancel project ownership | 已关闭 | R6 |
| F9 cancel/complete 原子终态 | 未关闭 | R6 的 requeue 竞态 |
| F10 notice 聚合 | 已关闭；预算存活另见 R3 | R9 |

总判定：FAIL（未闭合项：R1、R3、R5、R6、R7、R10、R11）
