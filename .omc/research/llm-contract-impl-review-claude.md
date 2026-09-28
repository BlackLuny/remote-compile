# 对抗式实现评审报告（Claude lane）

> 评审对象：五机制实现改动集
> 基准：`docs/proposals/llm-contract-mechanisms.md` v2 + `.omc/research/llm-contract-impl-report.md` + `grok-only.diff`（26 文件 / 4322 行）
> 日期：2026-07-28
> 所有发现均已回到仓库现行文件核对，不是只看 diff 推断。未修改任何代码文件。

---

## 一、必须本轮修复（BLOCKER）

### B1. 机制四实际上从不触发：`auto` 基线解析出的就是任务自己

`crates/rc-server/src/app.rs:888-899`、`crates/rc-server/src/store.rs:4043-4072`

`resolve_baseline` 的 SQL 是 `WHERE project_id=? AND worktree_id=? AND task_type=? AND status='done' ORDER BY finished_at DESC LIMIT 1`，**没有排除当前任务**。而 `task_status` 只有在 `result` 非空（即任务已 done）时才算 delta。任务一旦完成，它自己就是同键下 `finished_at` 最大的那一行，于是 `resolve_baseline` 返回它本身，app.rs 里的 `if base.id != t.id` 判 false，delta 被跳过。

结果：**刚跑完的任务永远拿不到 `diag_delta`**；只有在去查一个已经被更新任务超过的旧 task_id 时才会算出 delta——正好和反馈 #5 的场景相反。PR4 的验收（"同 worktree 增量三分类"）在端到端路径上不成立，尽管 `delta.rs` 的单测全绿。

**建议修复**：`resolve_baseline` 增加 `AND id != ?4`（当前 task_id）参数，并把 app.rs 里的 `base.id != t.id` 从"丢弃"改成"由 SQL 保证"。注意仅在应用层过滤不够——`LIMIT 1` 已经把次新的一行挡在外面了。

### B2. `CancelTask` 没有归属校验，任何 agent 可以取消任何项目的任务

`crates/rc-server/src/grpc_agent.rs:562-627`（`cancel_task`）、`crates/rc-server/src/grpc_agent.rs:29-40`（`authenticate`）

规格 §5.5 明确要求："归属校验：task 行的 `project_id` 必须与调用方 agent 的项目身份一致。" 实现里只调用了 `self.authenticate(&req)?`，而 `authenticate` 只校验全局 agent bearer token（`agent_token_valid`），不带任何项目维度。`CancelTaskReq.agent_session` 字段在 proto 里加了，agent 侧 `client.rs:22-28` 传的是 `String::new()`，服务端也从不读它。

持有任意有效 agent token 的调用方，只要知道（或猜到）task_id，就能杀掉别的项目正在跑的构建。控制面是共享的，这不是理论风险。

**建议修复**：从 `CancelTaskReq` 带上调用方的 project_id（或复用 agent_session→project 的既有映射），与 `task.project_id` 比对，不符返回 `PermissionDenied`。

### B3. 自动补救只在 `check` 的同步窗口内可达，对真实的长构建等于没有

`crates/rc-agent/src/engine.rs:373-421`、`crates/rc-agent/src/engine.rs:584-588`

`maybe_remediate` 被调用的位置在 `check()` 里 `client.get_task(&handle.task_id, wait)` 之后的 `if let Some(result) = &status.result` 分支内，`wait` 默认 4 秒（`default_wait_secs`）。而 `get_result` 路径（`engine.rs:584`，MCP `tool_get_result`）只有 `self.render(&status, 0)`，完全没有补救逻辑。

反馈 #2 的原始场景是一次 432 秒的构建被 OOM 杀掉。这种任务不可能在 4 秒内出结果，调用方拿到的是"⏳ 仍在执行"，随后用 `get_result` 轮询——而那条路径永远不会补救。PR2 验收项"OOM 自动降配重试一次并声明（#2）"在生产路径上不成立。

**建议修复**：把补救逻辑下沉到 `Engine::get_result`（或抽一个"终态后处理"公共入口，`check` 与 `get_result` 共用），并在跨轮询的场景下记住"本请求是否已重试过"——目前"重试恰 1 次"是靠 `maybe_remediate` 里 `no_cache: true` 的一次性调用保证的，跨轮询要另找地方存这个状态。

### B4. 单元进度仍然逐条写 `task_events`，与 §5.2/F26.3 直接冲突

`crates/rc-worker/src/runner.rs:242`、`crates/rc-server/src/grpc_worker.rs:246-258`

grpc_worker 侧加了 `if !p.phase.is_empty()` 作为"不写 task_events"的守卫，但 worker 侧 emitter 发的是 `client::progress_event(&emit_task_id, "building", &p.current_unit)`——phase 恒为 `"building"`，非空。于是每一次节流后的进度上报（≥2s 一次）都会执行 `add_timeline(...)` + `set_status(...)` + `publish_task(...)`。

一次 432 秒的构建 ≈ 200 行 task_events + 200 次单控制面写锁上的 `set_status` + 200 次广播。这正是 F14.4/F26.3 要消除的行膨胀与写锁压力，规格原文："server 不把每条进度写 task_events"。实施报告"进度不写 task_events"的表述与实际行为不符。

**建议修复**：进度事件不要复用 `progress_event`，新增一个 phase 为空的 unit-progress 事件（或在 `TaskProgress` 上加 `is_unit_progress` 标记），让 grpc_worker 的守卫真正生效。

### B5. `set_status` 无终态守卫，进度事件可以把已取消的任务"复活"

`crates/rc-server/src/store.rs:628-636`、`crates/rc-server/src/app.rs:657-660`

`set_status` 是无条件 `UPDATE tasks SET status = ?2 WHERE id = ?1`，不检查当前状态是否终态。结合 B4：cancel 把任务置为 `canceled` 并写入 CANCELED verdict 之后，worker 那边还在飞的进度事件（发出时尚未收到 `CancelTaskId`）到达 grpc_worker，执行 `set_status(task, "building")`，把状态从终态改回非终态。

后果是连锁的：`on_task_done`（app.rs:657）依赖 `is_terminal(task.status)` 丢弃迟到结果，状态被改回去之后这个丢弃失效，worker 真正的结果会覆盖掉 cancel 写入的 verdict。规格 §5.5 依赖的正是"late result 由现有 terminal 丢弃逻辑处理"。

改动前这个竞态窗口很窄（进度事件只在 syncing/building/uploading 三个相位转移时发生），机制五把它变成了每 2 秒一次的常态窗口。

**建议修复**：`set_status` 加 `AND status NOT IN ('done','failed','canceled','superseded')`。这一条即使 B4 修好也应该做——它是取消语义的正确性底线。

---

## 二、需要本轮修复（MAJOR）

### M1. 预算门没有覆盖所有 MCP 响应出口

`crates/rc-agent/src/mcp.rs:107-121`（dispatch）、`:150-161`（仅 check）、`:200-208`（仅 get_log）

规格 §3.1 要求 rc-agent 建一个"**所有 MCP 响应文本必经**的渲染出口"。实际只有 `check` 和 `get_log` 过门。`get_result`（`mcp.rs:165-173`）直接 `.map(|o| o.text)`——而它渲染的是和 `check` 完全相同的 `format_result` 输出（含诊断、env_hints、delta 块）。轮询是长构建的主要交互面，也是最容易灌爆上下文的那一个。`get_build_profile`/`list_envs`/`prepare_env`/`get_env_status`/`list_workers`/`cancel` 同样未过门。

**建议修复**：在 `call_tool` 的 `let text = match name {...}` 之后统一 `if self.engine.cfg.budget_gate { text = gate_response(&text) }`，同时删掉 `tool_check` 里的重复调用。

### M2. 分槽预算是死代码；实际截断是"从尾巴一刀切"

`crates/rc-core/src/budget.rs`（`Slot` / `SlotPiece` / `assemble`，diff 行 1341-1404）

全仓库检索确认 `assemble` / `SlotPiece` / `Slot::` 在 `budget.rs` 之外**零调用**。生效的是 `gate_response`：逐行 elide 后若总量仍超 8KB，就 `truncate_bytes(&joined, RESPONSE_BUDGET)` 尾部截断。

§3.1 的优先级（headline+证据 > 诊断 > Critical 通知 > Info/Warning > 装饰）因此没有任何机械保证。由于通知是在 `check()` 末尾用 `with_note` 追加到文本尾部的（`engine.rs:376`），**Critical 通知恰恰位于最容易被切掉的位置**——exclude、egress_pending、egress_refused 这些"影响结果解释"的信息，在诊断量大的响应里会被静默丢弃，而这正是 §3.2 判定"不赌调用方记性"的那一类。

实施者自列为 PR3 偏离 1 并评价为"语义接近"。不同意：headline 保住了，Critical 没保住，而"Critical 每次必显"是 F13.2/F23 的裁决结论。

**建议修复**：把通知拆成 SlotPiece 走 `assemble`；最简做法是截断后把 Critical 通知重新拼回尾部。

### M3. `line_byte_offset` 行内续读是死胡同，会让调用方陷入循环

`crates/rc-agent/src/mcp.rs:222-230`、`crates/rc-agent/src/mcp.rs:479-490`（工具 schema）

超长单行时响应输出的续读指引是 `… 行内续读: get_log(task_id="...", offset=N, raw=true, line_byte_offset=M)`，但 `get_log` 的 `inputSchema` **没有声明** `line_byte_offset`，`tool_get_log` 也**从不读取**它（全仓库检索：只有 proto 定义、budget.rs 内部字段、以及这一行 format 字符串）。调用方照做会拿回一模一样的前 8KB，然后被告知再来一次——无限循环。

§3.1/F22.1 明确要求"单行超出响应上限时，响应携带 `line_byte_offset/next_byte_offset` 支持行内续读"。实施报告 PR3 偏离 2 称"仅 agent 预算门实现"，但 agent 侧同样没有读取端。

另外 `gate_log_lines` 的续读分支只在 `raw && lines.len() == 1` 时进入，默认 `limit=100` 下永远进不去。

**建议修复**：要么真做（解析参数 → 传给 `LogQuery.line_byte_offset` → server 在 `log_lines` 做行内切片），要么撤掉那句指引换成"该行过长，请用 grep 缩小范围"。不能保留一条会让 agent 打转的指令。

### M4. 测试通过时没有 TestSummary，`#3` 的绿色路径没有兑现

`crates/rc-core/src/diag.rs:642-661`（规则 0）、`crates/rc-agent/src/engine.rs:1320-1335`

`classify_facts` 的规则 0 在 `exit_code == 0` 时直接 `return Classification::with_verdict(...)`，而 `with_verdict` 把 `test_summary` 固定设为 `None`。只有规则 4（test_failed）和规则 10（unknown）会挂上 `test_summary`。

于是一次全绿的 `cargo test` 得到的结论是 `✓ success` 或 `✓ success, 3 warnings`——**没有 pass/ignored/binaries 计数**。§2.3 的渲染范例 `✓ 测试通过：47 passed, 2 ignored（3 个二进制）` 和 PR2 验收项"test 结论含 pass/fail"在成功路径上都不成立。`TestContract::render_outcome` 里那段正确的渲染代码是死的（见 M5）。

**建议修复**：规则 0 里当 `facts.test_summary` 存在时挂上去；`format_result` 已有渲染分支，会自动生效。

### M5. `TaskContract` 的 parse/render 是死代码，F24 的 parser 门禁在生产路径上不存在

`crates/rc-core/src/contract.rs`（`parse_outcome` / `render_outcome`）

全仓库检索：`contract::for_task` 只在 `engine.rs:455` 和 `engine.rs:878` 被调用，用到的只有 `default_command()`、`default_env()`、`remediation()`。`parse_outcome` 和 `render_outcome` **除 contract.rs 自己的单测外没有任何调用者**。

后果是 §2.2 的 F24 门禁（"libtest 解析仅在命令是本契约生成的默认命令时启用"）在实际链路上不成立：worker 的 `classify_with_exec`（`diag.rs:1093-1120`）无条件对全量日志跑 `parse_test_summary`，不知道命令是否被覆盖，`command_is_default` 参数在生产中从未被传过。实施报告"libtest 解析仅在「命令为本契约默认」时启用（F24）"与代码不符。

实际危害目前有限（nextest 不输出 `test result:` 格式，`summary_seen=false` 会自然回退），但规则 4 是产出 `ATTR_CODE` 的两条硬证据规则之一，它的输入门禁不该只存在于单测里。

**建议修复**：把 `command_is_default`（`assignment.command_override.is_empty() && profile.tasks 未覆盖 task_type`）透传到 worker 分类路径，或在 `TaskAssignment` 上带一个布尔位。

### M6. Notice 状态机被"一个 category 多条通知"打穿；baseline-off 定级过低

`crates/rc-agent/src/engine.rs:1129-1141`（scanner 通知）、`crates/rc-core/src/notice.rs`（`present`）

**(a) 状态键碰撞。** `collect_notices` 对每一条 scanner 警告都 `Notice::new("scanner", ...)`，共用同一 category。而 `NoticeState::present` 的状态键是 `(project, worktree, category)`，循环里逐条 `self.last.insert(key, n.identity)`——后一条覆盖前一条。两条警告的稳定快照会得到：第一条 prev=id2≠id1 → 全文；第二条 prev=id1≠id2 → 全文；**每次调用都全文重复，永远不会静默**。这正是 §3.2 要消灭的 `engine.rs:879-881` 式噪音，只是换了个地方。

**(b) 定级。** §3.2 把 "baseline-off" 与 exclude、egress-refused 并列为 Critical（"影响结果解释的"）。实现给所有 scanner 警告的是 `NoticeSeverity::Warning`，而 `scanner.rs:183` 产出的正是 baseline 关闭的警告。当它是唯一一条 scanner 警告时（常见情况），(a) 的碰撞不触发，它会从第二次调用起被**完全静默**——比噪音更糟。

**建议修复**：category 按语义细分（`scanner_baseline` / `scanner_no_git` / `multiroot_untracked`），baseline-off 定 Critical；或让 identity 覆盖整个快照集合而非单条。

### M7. 双败时用第二个 task_id 渲染第一次结果，第一次的 task_id 直接丢失

`crates/rc-agent/src/engine.rs:471-489`

```rust
let first_text = format_result(
    &status.task_id, // will re-format first below
    first, ...
);
// We need the original first task id — use fingerprint path note.
outcome.text = format!("{first_text}\n⚠ 自动补救仍失败（第二次 task_id={}）\n{}", status.task_id, ...);
```

`status.task_id` 是**第二次**任务的 id，却被用来渲染**第一次**的 result。`format_result` 会在 headline 下面打印 `task_id={task_id}`（`engine.rs:1277`），所以调用方看到的是：主结论标着第二次的 id，下一行又说"第二次 task_id=<同一个 id>"。第一次任务的 id 无处可寻，而 §2.5 要求"报首次 verdict 为主结论，但附带第二次的 task_id 与证据行"——首次日志反而成了取不到的那一份。

代码里两行注释（`// will re-format first below`、`// We need the original first task id`）是留在成品里的未完成思考，应一并清掉。

**建议修复**：`maybe_remediate` 增加 `first_task_id: &str` 参数（调用点有 `handle.task_id`）。

### M8. `task=test` 的补救 env_patch 是空操作，重试必然重复失败，且声明失实

`crates/rc-core/src/contract.rs`（`resource_remediation`、`TEST_DEFAULT_ENV`）

`TestContract::default_env()` 已注入 `CARGO_PROFILE_TEST_DEBUG=0` 与 `CARGO_PROFILE_DEV_DEBUG=0`；`resource_remediation` 的 env_patch 恰好是同样这两个键值。对 test 任务，补救后的 effective env 与首次**逐字节相同**，fingerprint 也相同（靠 `no_cache: true` 才会真的重跑）。

于是白白烧掉一次完整构建（§2.5"重试成本有界"的论证前提被破坏），并对调用方声明"已自动以 CARGO_PROFILE_*_DEBUG=0 重试"——这句话在 test 场景下是假的，值本来就是 0。check/build 契约的 default_env 为空，补救对它们才有实际效果。

**建议修复**：`maybe_remediate` 里比较 patch 前后的 effective env，若无变化则不重试，改为在结论里说明"已在最低 debuginfo 配置下失败，请降低并行度或申请更大内存"。

### M9. `App.progress` 只增不删，控制面内存无界增长

`crates/rc-server/src/app.rs:54`、`:112-125`（`update_progress`）

`progress: Mutex<HashMap<String, ProgressSnapshot>>` 以 task_id 为键，在 `update_progress` 里 `entry(...).or_default()` 插入，**没有任何删除点**。任务终结时不清理，重启才归零。§5.2 说的是 "per-task 内存内 progress_snapshot"，隐含生命周期与任务同寿。量级不致命但确实是泄漏，且 `task_status` 每次都要锁这张只增的表。

**建议修复**：在 `on_task_done` / cancel 的终态路径上 `self.progress.lock().remove(task_id)`。

### M10. 读时 delta + history_ref 挂在长轮询的每一次唤醒上，且正则每次重编译

`crates/rc-server/src/app.rs:877-911`、`crates/rc-core/src/delta.rs`（`normalize_spans`）

`task_status` 的调用频率远高于"每次 get_task"：`grpc_agent.rs:284-306` 的长轮询循环里**每收到一个该任务的广播事件就调用一次**。结合 B4（每 2 秒一个进度事件），一次 432 秒的构建期间约调用 200 次，每次都：(1) 跑一次 `history_ref` 的 SQL；(2) 任务终结后重算 delta——包括 `resolve_baseline` 的 SQL、baseline `result_json` 的完整反序列化、以及 `compute_delta`。

而 `normalize_spans` **每次调用都 `Regex::new` 两个正则**，`strict_key` 每次调用它，`compute_delta` 里对 current/baseline 各跑 3 遍 strict_key。50+50 条诊断 → 每次 delta 约 300 次正则编译。

**建议修复**：正则用 `OnceLock`（`progress.rs` 的 `unit_re()` 里已有正确写法可照抄）；delta 只在终态时算一次并考虑随 result 落库或加短期缓存；`history_ref` 只在非终态时查。

### M11. 服务端仍然信任客户端送来的 `profile.canonical`，"执行 ≠ 指纹"的路径没有关掉

`crates/rc-core/src/fingerprint.rs:136-148`（`compute_for` 用 `&profile.canonical`）、`crates/rc-server/src/app.rs:226-234`

这是对实施者自列偏离 PR2-1 的风险裁定。

**先说通过的一半**：agent 侧的同源性成立。`Resolution::canonical()`（`profile.rs:249-251`）确实把 `p.env` 逐项写进 canonical；`to_pb()` 同时输出 `env` 与 `canonical`；worker 侧 `RustAdapter::cache_config`（`adapter.rs:191-193`）与 `GenericAdapter::cache_config`（`adapter.rs:33`）都把 `profile.env` 最后一层折进容器 env。所以"合并产物写回 profile.env 后 canonicalize"和"请求 env 真的到达 worker 容器"两条**通过**，F21.3 的"同 manifest 仅 env 不同 → fingerprint 不同"也成立。

**未通过的一半**：server 的"权威重算"只是 `compute_for(root_hash, profile, anchor_mount)`，而它读的是 **profile 里那串客户端自己写的 `canonical` 文本**，从不由 `ResolvedProfile` 的其余字段重新推导。`SubmitTaskReq.env`（新加的 field 15）服务端**一个字都不读**（已检索确认 app.rs 无引用）。因此一个撒谎的 agent 可以送 `profile.env = {恶意}` 而 `profile.canonical` 里写着诚实的 env——worker 执行前者，缓存键来自后者。`find_cached_result`（`store.rs:531-545`）只按 fingerprint + egress_key 查，**不带 project 维度**，毒化结果会被别的项目命中。

这正是 §2.4/F21 那句"禁止任何『hash 一份、执行 env 另一份』的实现"要堵的洞。它在改动前就存在（path/features/command 都有同样暴露），但改动把 env 从"仓库配置文件里的东西"提升成了"MCP 请求参数"，攻击面从"能改仓库配置"降到了"能发一次 check 调用"。

**建议修复**：不需要在 admit 里做完整 resolve——用收到的 `ResolvedProfile` 字段重新生成 canonical 并与客户端送来的比对，不一致就拒收（或以服务端版本为准）。十几行，且能一并关掉 path/features/command 的同类暴露。

---

## 三、可接受但需留档（MINOR）

| # | 位置 | 问题 |
|---|------|------|
| m1 | `rc.proto:222-224`、app.rs | `SubmitTaskReq.env = 15` 服务端完全不读，是静默 no-op 字段。目前无害（agent 已折进 profile.env），但对第三方客户端是陷阱：设了 env 却不生效也不报错。建议接线（配合 M11）或在 proto 注释写明"仅供审计，语义以 profile.env 为准"。 |
| m2 | `mcp.rs:202`、`app.rs:829-838` | 用了 `grep` 时 `chunk.offset` 是**过滤后列表**的下标，但 elide 标记打的是 `log:N`，冒充原始日志行号。§3.1/F13.3 要求"行号与 server 原始行号对齐"。`LogChunk.line_no` 字段加了但从未填充。 |
| m3 | `engine.rs:1299`、`app.rs:829` | verdict 证据的 `line_no` 是 1-based（`diag::line_no_of`），`get_log` 的 `offset` 是 0-based。照着"证据 (log:1847)"去 `get_log(offset=1847)` 会落在 1848 行。差一行，但证据行号的全部价值就是"直达"。 |
| m4 | `engine.rs:452-458` | 补救时 `patch` 先 clone 请求 env、再用 `rem.env_patch` 覆盖，即**补救会踩掉用户显式指定的 env**。§2.3 写的是"用户覆盖永远赢"。语义上可辩，但应在 note 里说明已覆盖。 |
| m5 | `engine.rs:463-465` | 补救重提交后没有处理 `handle.status == "needs_blobs"`（主路径 `engine.rs:341-351` 有处理）。`let _ = known; // reserved for blob re-upload` 是知情不做。命中概率低，但结果是重试任务卡死直到 wait 超时。 |
| m6 | `diag.rs:361-378` | `kind_from_verdict` 除单测外无调用者：实际 kind 由每条规则各自硬写。映射表因此可与规则表悄悄漂移，§1.3 想要的"逐格写死"只写在了测试里。建议让规则只产 verdict，kind 一律由 `kind_from_verdict` 导出。 |
| m7 | `grpc_agent.rs:581` | 取消写入 `kind: "infra_error"`。`app.finish` 的 metrics 按 kind 计数（§1.3 提到 kind 是 metrics 契约），用户主动取消会被计成基础设施故障，可能污染运维告警。 |
| m8 | `grpc_agent.rs:594-598`、`store.rs:702-707` | 取消走 `complete_task(id, "canceled", &canceled, "", "")`，log_ref 传空串，而 SQL 是 `log_ref = ?5` 无条件赋值——会清掉已有 log_ref（`image` 有 `CASE WHEN` 保护，log_ref 没有）。取消一个已上传日志的任务会丢失日志引用。 |
| m9 | `delta.rs`（fixed_count 段） | `fixed_count` 连算两遍：第一段 if/else-if/else 三个分支里有两个返回同值，随后立刻被第二段覆盖。功能正确但是死逻辑，中间那段注释（"Still report fixed when only rename heuristic fired? Spec: ..."）是留在成品里的自问自答。 |
| m10 | `store.rs:4053-4056` | `resolve_baseline` 的显式 task_id 分支 `return self.get_task(mode)` 没有归属校验。当前 server 恒传 `"auto"` 所以不可达，但 PR4 偏离 1 说的正是要把 `baseline` 参数下传——接线时会立刻变成跨项目诊断读取。 |
| m11 | `runner.rs:236-247` | emitter 节流是"距上次发送不足 2s 就 `continue`"，被跳过的更新不会补发。watch 是 latest-value 通道，正确写法是 `tokio::select!` 配 2s 定时器。实施者自列（PR5 偏离 2）：末尾长时间编译单个 crate 时进度会停在过时的 crate 名上。 |
| m12 | `docker.rs`（`inspect_exec_evidence`） | `worker_killed` 只在超时路径置位；`Runner::cancel → sandbox.kill` 路径不置位。§1.4 写的是"仅由 worker 在自己 kill（**超时/取消**）的代码路径置位"。它不是分类主键（F19），影响有限。 |
| m13 | `engine.rs:1183-1187` | `local_cache_allowed` 上方约 8 行解释"为什么不能本地缓存 egress 相关结果"的注释被删掉了，函数体一字未动。这是与本次改动无关的附带损伤，而仓库的提交历史表明这类论证是有意留下的。建议恢复。 |
| m14 | `diag.rs:1245-1258` | `fixture_test_abort_without_summary_is_unknown` 只断言 `!= AttrCode`，真正的结论用 `if rule(&c) == "unknown"` 包着——规则没命中时断言自动跳过，是个会永远绿的测试。测试体里还留着三行推演注释。§1.7 要求"test abort 无摘要 → UNKNOWN"，应直接断言 `rule == "unknown"`。 |
| m15 | `budget.rs`（`elide_line` 及其单测） | `elide_line` 输出是 `head(250) + 标记(≈46B) + tail(100)`，行号/长度位数较多时会略超 400B；它自己的单测断言的是 `<= LINE_BUDGET + 80`。§3.1 契约测试要求"非 raw 模式无单行 > 400B"。要么把 head 调到 250-标记长度，要么把规格里的 400 明确为"正文 400"。 |
| m16 | `engine.rs:598-602` | 进度行的"已运行 Ns"用 `created_at` 算，包含排队时间。§5.3 的语境是构建耗时（与 `build_ms` p50 并列展示），两者口径不一致会让"参考"更不可比。 |
| m17 | `engine.rs:244-257` | 本地缓存命中路径调用 `collect_notices` 时 egress_pending/egress_refused/warnings 都传空切片，于是这些 category 在状态机里被判"消失"，下次出现按首次全文重播。方向是安全的（多说一次），但会让 Critical 通知在缓存命中/未命中交替时反复全文。 |

---

## 四、NIT

- `progress.rs`（`render_progress`）：`format!("⏳ building")` 无参数，`clippy::useless_format`。
- diff 里含 `crates/.DS_Store`、`crates/rc-server/.DS_Store` 的二进制变更。
- `diag.rs:706-712`：规则 6 的行号计算里 `find_line_in_tail`（反向找最后一处）与 `line_no_of`（正向找第一处）混用，`.max(n as u32)` 之后又被下一行的 `if abs > 0 { abs }` 完全覆盖——前面那次计算是死的。

---

## 五、覆盖声明：本轮核对为**符合规格**的部分

**§1.5 规则表** — 规则 0–10 的顺序、命名与规格表**逐行一致**；timeout 在诊断之前求值且只看 `timed_out`（F19），`timeouts_win_over_everything` 语义保留；`ATTR_CODE` 只由规则 3、4 产出，二者都要求硬证据；规则 3 在 `error_count > 0 && !all_environmental` 时直接返回，raw marker 无法改判（`fixture_compile_error_plus_env_marker_stays_code` + `a_compile_error_is_never_offered_a_package` 均在），F9 不变量保住；SIGKILL 用的是 cargo 精确串 `(signal: 9, SIGKILL: kill)` 而非宽松子串（F8 收紧要求）；v1 的"test 崩溃 → CODE"启发式已删除。

**§1.3 映射表** — `kind_from_verdict` 七行与规格表格逐格吻合并有 `kind_mapping_table_is_complete` 覆盖；可缓存集合 `{success, compile_error}` 在服务端（`store.rs:543`）与 agent 本地（`index.rs:180-184`，F12 要求的新增过滤）两侧一致；RESOURCE/UNKNOWN → env_error 因而天然不可缓存；磁盘满归 INFRA（走换机重试）而非 RESOURCE；规则 2 与规则 9 都继续填充 `env_hints`（F17）。

**§1.4 证据采集** — `inspect_exec_evidence` 的调用位置正确：在 `run_created` 返回后、`remove_container` 之前，且只在 run 成功返回时执行。

**§1.7 fixture 清单** — 十个 fixture 全部存在（OOMKilled、仅日志 SIGKILL、worker 超时 137 且断言 ≠ RESOURCE、rustc SIGSEGV、test abort 无摘要、磁盘满、纯编译错误、编译+env marker 混合、测试失败摘要、全 env 诊断）。质量上只有 m14 一条不合格。

**§2.4 分层与 denylist** — 合并顺序 adapter < contract < profile < request 与规格一致；denylist 只作用于请求 env，profile 仍可覆盖 `SCCACHE_*`（`profile_may_set_sccache_but_request_may_not` 直接测了 F11.2）；key 正则、单值 4KB、32 条上限齐全。**env 确实进 canonical**（`profile.rs:249-251`）**且确实到达容器**（两个 adapter 的 `cache_config` 都把 `profile.env` 折在最后一层）——这两条是本节最关键的验证，均通过。

**§2.4 ABI** — `EXECUTOR_ABI` abi2→abi3，bump 触发器清单写进 `fingerprint.rs` 文档注释，`abi3_does_not_match_abi2_fingerprint` 验证旧指纹不命中。

**§2.5 白名单** — `auto_remediate_allowed` 封闭为 `{oom_killed, sigkill_suspected_oom}`，磁盘满/超时/未知均返回 None 并有测试；重试上限 1 次；`auto_remediate` 配置开关与单请求 `no_remediate` 都接线了。（可达性问题见 B3，白名单本身正确。）

**§4.2 身份键** — `normalize_spans` 只剥 `\S+\.rs:\d+:\d+` 与裸 `:\d+:\d+` 尾缀，`[u8; 32]` 与 "expected 2 arguments" 里的数字保留并有针对性单测（F25.1）；顶层行号不入键，行号漂移判为 preexisting；任一侧 truncated 或存在空 code 或疑似 rename → `approximate=true`；truncated 时 `fixed_count` 强制归零（F6.3）；fuzzy 只抬 approximate、不计入 new/fixed。

**§4.3 基线键** — `(project_id, worktree_id, task_type)`，SQL 里三个条件都在，绝不跨 worktree 借用（F5）。（选中自己的缺陷是 B1，键的设计本身正确。）

**§5.4 长轮询** — `return_on_progress` 是 presence 明确的 bool，默认 false，旧 agent 不发新字段时代码路径与今天完全一致；`progress_version` 游标由客户端回传，闭环成立。

**§5.5 取消顺序** — 严格镜像 admin 路径：先 `complete_task` 写终态 + server 自己写 CANCELED verdict，再向 worker 发 `CancelTaskId`；`classify` 全部十条规则无一产出 `Status::Canceled`；未复用 supersede。（缺的是归属校验 B2 与终态守卫 B5。）

**§6 兼容性** — proto 全部 additive（新 message + 新字段号，无重编号、无删除）；四个新 `TaskResult` 字段都在 `build.rs` 加了 `#[serde(default)]`，旧 `result_json` 可反序列化；`format_result` 保留了无 verdict 时的旧 kind 渲染路径；schema 迁移遵循仓库既有的 "fresh DB 跳过全部 steps" 模式（`store.rs:383-392`），新库不会因重复列而失败——特地核对过，是对的。

**§6 kill-switch** — agent 与 server 两侧五个开关都在，默认全开，`#[serde(default = "default_true")]` 保证旧配置文件可读。

---

## 六、结论

**必须本轮修复**：B1（delta 从不触发）、B2（cancel 无归属校验）、B3（补救不可达）、B4（进度写 task_events）、B5（cancel 竞态）、M1（预算门未全覆盖）、M2（Critical 会被截断）、M3（行内续读死胡同）、M4（绿色测试无计数）、M5（F24 门禁不在生产路径）、M6（Notice 状态机被打穿 + baseline-off 定级）、M7（第一次 task_id 丢失）、M8（test 补救空转）、M9（progress 表泄漏）、M10（读时 delta 性能）、M11（canonical 未重算）。

其中 B1、B3、M4 三条的共同点值得单独提出：**它们都不是逻辑写错，而是"单测全绿、端到端不通"**——`delta.rs`、`contract.rs`、`budget.rs` 的单测覆盖相当扎实，但三个机制的生产入口分别断在 SQL 的一个缺失条件、一个函数的调用位置、和一个提前 return 上。实施报告的"自验：全绿"因此是真实的但不充分。建议本轮补的不是更多单元测试，而是三条端到端断言：跑完一次 check 后响应里必须有增量块；OOM 的长任务在 `get_result` 轮询下必须看到补救声明；绿色 test 的结论必须含 passed 计数。

**可接受留档**：MINOR 全部 17 条 + NIT 3 条。其中 m2/m3（日志行号口径）建议与 M3 一并处理，因为它们共同决定"证据行号能不能直达"这个 I1 的实用价值。

### 对实施者自列偏离的裁定

| 偏离 | 裁定 | 理由 |
|------|------|------|
| PR2-1 server 未同源 resolve | **需本轮修复**（M11） | "执行 ≠ 指纹"路径确实仍开放，且 fingerprint 缓存全局不分项目；env 从配置文件升格为请求参数后攻击面显著降低。最小修复是重算 canonical 并比对，非完整 resolve。 |
| PR3-1 未严格分槽 assemble | **需本轮修复**（M2） | 不是"语义接近"：通知被追加在文本尾部，尾部截断恰好先切 Critical，而"Critical 每次必显"是 F13.2/F23 的裁决。 |
| PR4-1 baseline 参数未下传 | **可接受留档** | 规格默认语义就是 `auto`，MCP 参数已声明但未接线属功能未完整而非行为错误。接线时须一并加 m10 的归属校验。（但 B1 使得连 `auto` 都不工作，那是另一回事。） |
| PR5-1 未默认开 return_on_progress | **可接受留档** | 旧轮询行为保留，进度信息通过普通 `get_task` 一样能拿到，纯优化项。 |
| PR5-2 emitter 丢最后一次进度 | **可接受留档**（m11） | 影响是进度显示停在过时 crate，不影响终态结果。 |
| kill-switch 未贯穿 worker | **可接受留档** | 但 `unit_progress` 是例外：关掉它并不能止住 B4 的 task_events 写入，因为守卫在 worker 之后。修 B4 时顺手让 server 侧开关也能短路 `update_progress`。 |
