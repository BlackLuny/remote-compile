# 对抗式设计评审：`docs/proposals/llm-contract-mechanisms.md`

> 基准：工作区当前状态（2026-07-28）  
> 先读：`docs/DESIGN.md` §1（背景、目标、§1.1 原则、§1.2 非目标）  
> 已核实代码：`diag.rs` classify；`docker.rs` 355–390；`adapter.rs` 139–201；`engine.rs` 866–955、1010–1086；`store.rs` 690–719 + `schema.sql`；`fingerprint.rs`  
> 约束：只读代码；禁止改除本文件外的任何文件；禁止 acp-agent MCP

---

## F1 — `status+attribution → ResultKind` 映射表缺失，协议兼容不可实施

**严重度：** BLOCKER  
**维度：** 事实 / 协议兼容 / 总体耦合  
**设计章节：** §1.2、§1.4、§6

**证据：**

- 设计写：`TaskResult` **保留** `kind`，由 `status+attribution` 推导；旧读者不受影响。  
- 未给出任何映射表。当前全链路仍以 `ResultKind` 字符串为契约：
  - `rc.proto` `TaskResult.kind` 注释：`success|compile_error|env_error|infra_error|timeout`
  - `model.rs` `is_retryable` 仅 `InfraError`；`is_cacheable` 仅 `Success|CompileError`
  - `store.find_cached_result`：`result_kind IN ('success','compile_error')`
  - `app.finish` 按 kind 记 metrics / image outcome / profile outcome
  - `format_result` / `agent_hint` 只认五种 kind
- 新轴 `ATTR_RESOURCE`（OOM/磁盘）在旧 kind 中无对应格：
  - 映到 `compile_error` → 继续诱导向改源码（正是本提案要修的 #1）
  - 映到 `env_error` → `agent_hint` 仍是「缺依赖 / prepare_env」
  - 映到 `infra_error` → 触发换机重试（`app.rs` on_task_done），OOM 换机通常无用且烧容量
  - 映到 `timeout` → 语义撒谎
- `DESIGN.md` §3.5 写明 **磁盘满 = infra_error**（自动换机重试）；提案规则 6 把 `No space left` 定为 `ATTR_RESOURCE`，与设计主文档冲突且未声明谁优先。

**建议修改：**

1. 在提案中写死一张完整映射表（含 `UNKNOWN`、`RESOURCE`、`CANCELED`、`PROJECT_CONFIG`）。  
2. 明确：`RESOURCE` / `UNKNOWN` 的 **可缓存性**、**是否 retryable**、**metrics 桶**、**agent_hint 原文**。  
3. 与 `DESIGN.md` §3.5 对齐：磁盘满是换机重试还是资源归因；二选一写进迁移章节。

---

## F2 — PR1 重写规则表但依赖 PR2 的 `outcome`，test 失败归因会先回退

**严重度：** BLOCKER  
**维度：** 归因规则 / PR 顺序  
**设计章节：** §1.4 规则 9/10、§2.3、§6–§7

**证据：**

- 规则 9：`outcome 为 TestOutcome 且 failed>0 → ATTR_CODE`。  
- 规则 10 默认：`UNKNOWN`。  
- I1：`ATTR_CODE` **只能**由规则 5/8/9 产出。  
- 现网：`classify` 对 `TaskType::Test` + 非零 + 无诊断 → `CompileError`（`diag.rs:311-320`）；单测 `failing_tests_are_a_code_problem` 固定此行为（日志可无结构化诊断，仅有 `test result: FAILED...`）。  
- `cargo test` 默认命令无 JSON（`adapter.rs:163`），`parse_diagnostics` 走 `parse_generic`（`adapter.rs:172-178`），**经常 0 条诊断**。  
- PR 序：PR1 = 规则表；PR2 才引入 `TestOutcome`。§6 写「机制二 outcome 喂机制一」，却让 PR1 先合。

**可证伪后果：** PR1 单独合入后，真实测试失败在无规则 9 时落入规则 10 → `UNKNOWN`，相对现网 **「测试失败 = 代码问题」** 是功能回退；直到 PR2 才恢复。验收表写 PR1 只验 OOM，掩盖了 test 回归。

**建议修改：**

1. 把「无 JSON 的 libtest 摘要 → CODE」做成 **不依赖完整 TaskContract** 的规则（或把 TestOutcome 最小解析并入 PR1）。  
2. 或 PR 序改为：outcome 解析先于/同于规则表切换；禁止「半规则表」上线。  
3. 明确 PR1 过渡态：旧 test 分支保留，直到规则 9 有 fixture 绿灯。

---

## F3 — `ATTR_* → Remediation` 按归因粒度过粗，磁盘满会套 OOM 补救

**严重度：** BLOCKER  
**维度：** env / 自动补救 / 归因  
**设计章节：** §1.4 规则 3/6、§2.5

**证据：**

- §2.5：`verdict.attribution == RESOURCE` 且未重试 → 应用 `remediation(RESOURCE)`。  
- TestContract 的 RESOURCE 补救 = `CARGO_PROFILE_TEST_DEBUG=0`。  
- 规则 6：`disk_full → RESOURCE` 与 OOM 同属 RESOURCE。  
- 自动补救位置在 **agent 侧**、指纹随 env 变 → 磁盘满时会再跑一轮「关 debuginfo」的全任务，对 worker 盘满 **无效**，消耗同步/队列/编译预算，且结果声明「已自动降配重试」误导调用方。

**建议修改：**

1. 补救键改为 **规则名** 或 `(attribution, rule)`，禁止仅按 attribution。  
2. 磁盘满：不自动重试；remediation 指向清缓存/换 worker/infra，与 `DESIGN.md` §3.5 一致。  
3. 自动重试白名单显式列出：`oom_killed` only（或等价 rule name）。

---

## F4 — 「同 profile」无存储身份，基线与 `units_total`/ETA 不可实施

**严重度：** BLOCKER  
**维度：** delta 身份键与多分支基线 / 流式与 ETA  
**设计章节：** §4.3、§5.3、§6 schema

**证据：**

- 提案：`auto` 基线 =「同 profile 下最近一次完成任务」；`profile_build_stats(profile_id, n=20)`；`units_total` 取同 profile 上次 `units_compiled`。  
- `schema.sql` `tasks`：**无 `profile_id` 列**；仅有 `project_id`、`worktree_id`、`fingerprint`、`command`、`image`、`task_type` 等。  
- `record_profile_outcome(&task.project_id, "", ...)` 路径参数为空串——服务端本就没有稳定 profile 主键。  
- `fingerprint` 是内容寻址（含 manifest），**同 profile 不同树** 指纹不同，不能当 profile 键；**不同分支同树配置** 又可能共享 project 而无分支维度。

**可证伪后果：** 实现者只能退化成「同 `project_id`+`task_type` 最近任务」→ 多 worktree/多分支交叉污染（见 F5）；或根本无法写查询。

**建议修改：**

1. 定义可落库的 profile 键：例如 `blake3(canonical without command? 或 project_id‖path‖task_type‖image_digest‖env_hash)`，并写入 `tasks` 列 + 索引。  
2. 或放弃「同 profile」措辞，改成明确 SQL 谓词，并论证为何不会串分支。  
3. `units_compiled` 若只放 `result_json`，ETA 查询要扫描 JSON——写明是否加列及迁移。

---

## F5 — `baseline=auto` 跨 worktree/分支串基线，delta 可系统撒谎

**严重度：** MAJOR  
**维度：** delta 身份键与多分支基线  
**设计章节：** §4.3–§4.4、对照 DESIGN §1.1「内容寻址」

**证据：**

- DESIGN §1.1.2：缓存/去重 **不依赖 git 提交**；agent 多 worktree 并行是常态（§1 背景）。  
- 提案基线不绑定 `worktree_id` / `agent_session` / `base_commit` / manifest 祖先关系，只说「同 profile」。  
- 场景：worktree A 绿；worktree B 改 unrelated 文件首次 check——auto 取 A 的 SUCCESS 作基线 → B 的全部错误显示「本次新增」，`fixed_count` 对 B 无意义。  
- 反场景：同 worktree 连续失败，优先 SUCCESS 会跳过「上一失败」作基线，**相对上次迭代** 的增量（反馈 #5 更可能想要的）丢失。

**建议修改：**

1. `auto` 默认：`same worktree_id (+ task_type)` 最近完成；无则 `none`。  
2. 「上次绿」作为显式模式 `baseline=last_success`，不要做 silent default。  
3. 跨 worktree 基线必须显式 `baseline=<task_id>`。

---

## F6 — 诊断 identity 在 `code` 空与 normalize 后碰撞，计数配对 silently 错

**严重度：** MAJOR  
**维度：** delta 身份键  
**设计章节：** §4.2

**证据：**

- identity = `blake3(file_path ‖ code ‖ normalize(message))`。  
- `parse_generic`（test/无 JSON）`code` 恒为空（`diag.rs:88`）。  
- `normalize` 剥行号/列号并折空白后，同文件多处「mismatched types」类消息易合并为同一键；按出现次数配对在截断（`top_diagnostics` 50，`truncated_diagnostics`）下 **次数本身不可信**，`approximate=true` 只标截断、不标碰撞。  
- 行号不参与 identity（合理），但同键消歧仅靠展示行号——集合差无法区分「同消息两处」与「同一处重复上报」。

**建议修改：**

1. identity 附加 `level` + 规范化后的 `span` 弱特征（如相对函数名 hash，若可得）。  
2. 无 `code` 时降级：`approximate=true` 或禁用 delta，仅展示全量。  
3. 规定：任一侧 `truncated_diagnostics>0` **不报告 fixed_count**（避免假「已修复」）。

---

## F7 — 137 证据链：`worker_killed` 未接入现有取消/超时路径；规则 2 与 admin cancel 语义冲突

**严重度：** MAJOR  
**维度：** 归因规则 / 137 / OOM / 超时  
**设计章节：** §1.3–§1.4、§5.5

**证据：**

- `RunOutput` 仅有 `exit_code`/`timed_out`/stdout/stderr（`docker.rs:92-100`），无 OOM/signal/worker_killed。  
- 超时：`timed_out=true` + `exit_code=137`（`docker.rs:356-367`、`384-391`），**不**读 `OOMKilled`。  
- Admin 取消：`set_status(canceled)` 后 `CancelTaskId`（`admin.rs:623-635`）；`on_task_done` 若已 terminal **丢弃** late result（`app.rs:633-636`）。  
- 提案规则 2：`worker_killed && !timed_out → CANCELED`；§5.5 称复用 `app.rs:379-420` **supersede** 路径。  
- 但 `TaskState::is_cancelable` **仅** pending/syncing/queued（`model.rs:103-106`）；supersede 只清未执行任务，**不能**取消 running。  
- 真正杀容器的是 admin cancel / `Runner::cancel`（`runner.rs:493-497`），与 supersede 不是同一条路。

**可证伪后果：**

1. 按字面「复用 supersede」实现 agent cancel → running 任务停不掉。  
2. 走 admin 语义 → 无 TaskResult/verdict，规则 2 永不跑；「verdict=CANCELED」无处产生。  
3. kill 后若未先标 terminal，137 + 无 `worker_killed` → 可能被规则 3 的日志 `signal: 9` 或默认分支误伤。

**建议修改：**

1. Cancel 规范写成：**镜像 admin cancel**（先 terminal + kill），verdict 由 server 在 cancel 时写入，不依赖 worker classify。  
2. `worker_killed` 仅由 worker 在 **自己** kill（超时/取消）路径置位；inspect 在 `remove_container` 之前。  
3. 从 §5.5 删除「复用 supersede」表述。

---

## F8 — 规则 3 日志 `signal: 9` 旁路可误判；规则 5 启发式不可测试

**严重度：** MAJOR  
**维度：** 归因规则 / 137 / OOM / ATTR_CODE 规则 5  
**设计章节：** §1.4 规则 3/5、§1.6

**证据：**

- 规则 3：`oom_killed` **或** 日志含 `signal: 9, SIGKILL`。  
- 无 `OOMKilled` 时仅靠日志：cgroup/OOM 文案、测试框架 abort、手动 kill 都可能出现 SIGKILL 字样；**精度优先**承诺被「或日志」削弱。  
- 规则 5：`测试二进制 signal: 6/11` 且「`test result` 前无 rustc 上下文」——「rustc 上下文」无定义（crate 名？`Compiling`？`error: could not compile`？多二进制交错日志？）。  
- ATTR_CODE 仅 5/8/9：规则 5 假阳性直接违反 I1（无硬证据定罪）。  
- 混合日志（编译中 rustc SIGSEGV + 后续测试）规则 4 vs 5 顺序依赖未定义边界。

**建议修改：**

1. 规则 3：**仅** `exec.oom_killed==true` 定 RESOURCE；日志 SIGKILL 单独规则 → `UNKNOWN`+摘录，或 `RESOURCE` 但 `rule=sigkill_suspected` 且 **禁止**自动补救。  
2. 规则 5 推迟到有可靠解析（例如 libtest/JSON 或明确 `test binary ... exited with signal` 行）；否则并入 UNKNOWN。  
3. §1.6 fixture 必须含：仅日志 SIGKILL 无 OOMKilled；rustc SIGSEGV；test abort；worker timeout 137。

---

## F9 — 规则 8「指向源文件」与现网 env/compile 精判不一致，可能重开 risk #4

**严重度：** MAJOR  
**维度：** 归因规则 / ATTR_CODE 规则 8/顺序  
**设计章节：** §1.4 规则 7/8，对照 `diag.rs:258-305`、DESIGN §3.5

**证据：**

- 现网：`error_count>0` 时，**全部** error 皆 `is_environment_diagnostic` 才 env，否则 **一律** `CompileError`（即使夹杂 env 形诊断）——刻意防止藏起真编译错误。  
- 提案规则 8：`≥1 条指向源文件的 error 级结构化诊断 → CODE`；规则 7 迁移 `looks_like_env_error` / `is_environment_diagnostic`。  
- 「源文件」未定义：`.rs` only？含 `.c`？空 `file` 的 cargo 错误？  
- 若「任意 .rs error → CODE」先于「全 env」精判：`compile_error!("openssl...")` 在 `.rs` 上，现网靠 `is_environment_diagnostic` 的 `.rs → false` 保持 CODE；一致。  
- 若实现成「有源文件 error 就 CODE」而跳过「全为 Header/Library」分支：对 **非 .rs** 的 missing header（现网 env）仍 OK；但对 **混合** 情形与规则 7/8 顺序未写，实现可漂移。  
- 规则序 7 在 8 前：纯文本 env marker（`looks_like_env_error`）在 **已有非 env 结构化 error** 时，现网不会因日志里的 `openssl` 字样改判 env（`a_compile_error_is_never_offered_a_package`）。提案规则 7 若只看 raw log 命中 marker，**会在有真编译错误时改判 PROJECT_CONFIG**——与现单测相反，**分类错误更贵**。

**建议修改：**

1. 明文保留现网不变量：`error_count>0 && !all_environmental ⇒ CODE`，且 **禁止** raw-log env marker 覆盖该结论。  
2. 规则 7 拆成：7a 结构化 env 诊断；7b 无诊断时的 raw marker；7b 不得在存在非 env error 时命中。  
3. 「源文件」改为与 `is_environment_diagnostic` 对称的可执行谓词。

---

## F10 — 现状病根叙述有事实偏差（docker「一次性收完」、部分行号）

**严重度：** MAJOR（事实维度；会导致错误改造方向）  
**维度：** 事实  
**设计章节：** §1.1、§5.1

**证据：**

- 提案 §5.1：`docker.rs:335-353`「容器输出整体缓冲……一次性收完」。  
- 实码：`logs` + `follow: true` **流式** `next().await` 追加到 `stdout`/`stderr`（`docker.rs:323-354`）；进度不可见是因为 **没有中间 `TaskProgress` 上报**，不是 API 一次性 dump。  
- 超时杀容器段落行号 355–390 **正确**；OOM 不读 `OOMKilled` **正确**（全仓无 `OOMKilled` 引用）。  
- `classify` 默认 test 分支行号 314–325 与现码 311–320 略偏，结论方向仍对。  
- `format_result` 1010–1086、`describe_*` 866–955、`complete_task` 690–719、`idx_tasks_fingerprint` schema:146、`fingerprint::compute` 78–100、`command_for` test 163、`cache_config` 181–201：**属实**。

**建议修改：**

1. 改写 §5.1 病根为「流式已进内存缓冲，但未解析/未上报单元进度」。  
2. 避免把改造做成「从一次性 API 换成 attach」——attach 已在用；缺的是 **progress 事件与解析**。

---

## F11 — 契约默认 env 与 fingerprint/`EXECUTOR_ABI` 接缝有缓存投毒风险

**严重度：** MAJOR  
**维度：** env / fingerprint / denylist / debug=0  
**设计章节：** §2.3–§2.4、§6，`fingerprint.rs`、`profile.rs` canonical

**证据：**

- fingerprint 哈希 `profile.canonical`（`fingerprint.rs:85-98`）；canonical **含** `env[k]=...`（`profile.rs:249-251`），**不含** adapter `cache_config` 注入的 `RUSTC_WRAPPER`/`CARGO_HOME` 等（worker 侧 `adapter.rs:181-200`）。  
- 提案：`adapter 全局 < 契约 default_env < profile < 请求`；「合并后 env 全量参与 fingerprint」。  
- 若实现者把 `CARGO_PROFILE_TEST_DEBUG=0` 只放进 worker `cache_config`（与 sccache 同样「全局默认」风格）而 **未写入 Resolution.canonical**，则：旧结果（debuginfo=2）与新执行语义共享指纹 → **错误 cache hit**（正是 `EXECUTOR_ABI` 注释所防场景，`fingerprint.rs:56-68`）。  
- 仅靠 ABI 递增：若忘记递增且 env 未进 canonical，仍中毒。  
- denylist 拒绝覆盖 `RUSTC_WRAPPER`/`CARGO_HOME`/`SCCACHE_*`：若作用于 **profile env**（非仅请求 env），会打破现网「profile.env 覆盖 sccache 默认」行为（`adapter.rs:191-193` 后写覆盖）。提案未写 denylist 作用域。

**建议修改：**

1. 强制：契约/请求 env 必须进入 `Resolution`/`canonical` 后再算指纹；worker 只消费已解析结果。  
2. denylist 范围：仅 `SubmitTaskReq.env` / MCP 请求；profile 保持现状或单独「sandbox 不可变键」表。  
3. 清单化 ABI bump 触发器：default_env 语义变更必 bump，并写兼容测试「旧指纹不命中」。

---

## F12 — 默认 `CARGO_PROFILE_*_DEBUG=0` 与自动重试的运维/语义风险

**严重度：** MAJOR  
**维度：** env / 自动补救 / debug=0  
**设计章节：** §2.3、§2.5，对照 DESIGN §1.1.1 Token 经济

**证据：**

- 默认关闭 test/dev debuginfo 改变 **失败时 backtrace/调试信息** 与目标体积；对「要复现测试里的 debug assert / 依赖 debuginfo 的测试」是静默行为变更。  
- OOM 根因若是并行链接/超大 crate/测试运行期分配，debug=0 重试仍 OOM → 双倍队列时间；提案「两次都失败则报首次 verdict」丢弃第二次可能更有信息的日志。  
- agent 侧重试：server 侧相同 fingerprint 的 in-flight 去重（`find_active_by_fingerprint`）与第二次不同 fingerprint **各算各的**；多 agent 同时 OOM 会放大重试风暴。  
- 本地 `ResultCache.put` **不**按 kind 过滤（`engine.rs:367-369` + `index.rs:179`），现网 OOM 若被标 `compile_error` 会被本地缓存；RESOURCE 修复后若仍映射到 cacheable kind，agent 缓存继续毒化。

**建议修改：**

1. debug=0 作为 **opt-in 自动补救** 或 profile 推荐默认，而非全局 TestContract 强制；至少文档突破性变更。  
2. 本地缓存 put 与 server 相同：仅 success/compile_error（或新 cacheable 集合）。  
3. 自动重试上限、抖动、以及「报告两次 rule/证据」写进契约。

---

## F13 — BudgetGate 8KB 与 Notice/诊断/证据生命周期冲突，可吞掉 I1 证据

**严重度：** MAJOR  
**维度：** 预算门 / notices 生命周期  
**设计章节：** §3.1–§3.2、§0 I1/I3，对照 `engine.rs` 渲染拼装

**证据：**

- 默认 `max_response_bytes=8192`，单行 400。  
- 现网一次结果拼装：`format_result` + `describe_roots` + exclude/include + egress 三段 + `scan.warnings`（`engine.rs:322-357`、`866-955`）。egress 主机列表与多根路径可轻易数百～数千字节。  
- Evidence.excerpt、notice、诊断 **一律过同一道门**：截断顺序未定义——FIFO 截断会先砍诊断与「证据:」行，只留样板 notice，**直接违反 I1**。  
- `get_log` 的 BudgetGate 在 agent：server `get_log` 仍回原行（`app.rs:777-813`）；MCP 层截断后 offset 分页语义与「按原行号」错位未说明。  
- Critical notice「重复则压缩一行」：exclude 现网 **刻意每次全文**（`engine.rs:879-881` 注释：漏同步会呈现为普通缺文件错误）。改沉默/压缩后，多轮 MCP 对话中后期 check **不再提醒**，远程/本地一致性风险回潮——这是产品决策冲突，不是风格问题。

**建议修改：**

1. 预算分配优先级：headline + verdict/证据 + 诊断 > notices > 装饰；或分槽（诊断 4KB / notice 2KB / 其它）。  
2. exclude / egress_refused / baseline-off：**每调用必显**（或 `category` 级 sticky 策略表），不得套用 Info 静默。  
3. 分页：截断响应携带 `next_offset`/`bytes_omitted`；`get_log` 行号与 server 对齐。

---

## F14 — `seen_event_seq` 无存储模型；长轮询语义与现 `get_task` 不兼容

**严重度：** MAJOR  
**维度：** 流式落盘 / units_total / seen_event_seq  
**设计章节：** §5.2–§5.4

**证据：**

- `task_events` 表无 seq 列（`schema.sql:162-168`）；`timeline` 按 `at_ms, rowid` 排序（`store.rs:757-758`）。  
- `get_task` 长轮询 **仅在 terminal 时返回**（`grpc_agent.rs:286-297`）；progress 虽 `publish_task`（`grpc_worker.rs:248-253`），订阅循环忽略非终态。  
- 提案同时写「事件序号 > seen 时提前返回」和「默认 0 保持旧行为」。这两句按普通 proto3 标量语义互相冲突：旧 agent 不发送字段时服务端看到 0；只要已有任一 seq>0 的 progress，它就会提前返回，**旧 agent 实际被改变行为**。若实现特判 `seen==0` 为禁用，又没有 presence 位，新 agent 第一次订阅也无法表达「从 0 开始看事件」。  
- 响应 `TaskStatus` 也未设计 `event_seq`/`next_seen_event_seq`，客户端即使收到进度，也不知道下一次该提交什么游标；仅靠 timeline 的 `at_ms` 不唯一，当前排序还依赖 SQLite `rowid`。  
- 节流 ≥2s 的 unit progress 与 `task_events` 行膨胀、admin timeline UI 未评估。  
- `units_total` 用上次完成单元数：cargo 图变化、sccache Fresh 比例变化时分母错误，UI「23/107」可长期欺骗；`Fresh` 计数规则与 cargo 未保证稳定的人话格式耦合。

**建议修改：**

1. 事件序号：用 `rowid` 或显式 `seq INTEGER`，写入 `TaskStatus`。  
2. 长轮询：增加有 presence 的 `watch_progress`/`return_on_progress`，响应返回 `last_event_seq`；默认旧行为保留，**新 MCP get_result 显式打开 progress 返回**。  
3. `units_total` 标注 `estimate`；禁止在分母为估时渲染成确定分数，或改用「已见 N 个单元，无总量」。

---

## F15 — 流式进度与完整日志落盘的内存/协议双重成本未 bound

**严重度：** MINOR  
**维度：** 流式落盘 / 总体耦合  
**设计章节：** §5.2

**证据：**

- 提案：流式 attach **同时**「保留完整日志缓冲用于落盘，行为不变」。  
- 现状已全量拼 `RunOutput` 字符串；大 workspace 日志可达数百 MB 级内存尖峰——新设计不缓解。  
- 逐行 regex + progress RPC 增加 worker→server 流量；与「≥2s 一条」同存，但失败重连/重复 phase 未定义。

**建议修改：**

1. 日志改为落盘 spooled file + 上传，内存只留 ring buffer（分类用 tail 200 行）——若 YAGNI 则明确「已知 OOM 风险接受」。  
2. progress 幂等：同 `units_done` 不重复写 timeline。

---

## F16 — 五机制耦合过重；§8 追查率与 PR 切分 YAGNI/顺序问题

**严重度：** MAJOR  
**维度：** 总体耦合 / PR 顺序 / 简化 / YAGNI  
**设计章节：** §0、§6–§8、§7 表

**证据：**

- 单文档同时引入：新 verdict 轴、TaskContract trait、env API、agent 自动重试、BudgetGate、Notice 状态机、诊断 delta、流式进度、ETA 统计、Cancel RPC、admin 追查率。对照 DESIGN §1.1.5 **先简单后扩展**。  
- §6 顺序依赖写「四依赖一的 verdict 选基线」——基线实际可用旧 `result_kind=success`，**不依赖** verdict；依赖写错会误导排期。  
- 真依赖：规则 9→outcome（F2）；自动补救→正确 RESOURCE 分类（F1/F3）；delta 展示→BudgetGate 优先级（F13）。  
- §8「结论后 10 分钟 get_log 率」：需 agent 身份关联、时钟、隐私/多租户非目标下的噪声；与五个机制层无强绑定，属额外控制面范围。  
- 前置「先落地 egress 再动工」正确（工作区 `egress.rs` 未提交），但未估计与 Notice 状态机的交互（egress notices 正是 §3.2 动机）。

**建议修改：**

1. 最小可运维切片：**仅 PR1′ = ExecEvidence + OOM/timeout 分判 + kind 映射 + 不可缓存 RESOURCE**；不重写全文规则表。  
2. TestOutcome、BudgetGate、delta、streaming 分 PR，各有独立可回滚开关。  
3. 删除或降级 §8 到「以后」；修正 §6 依赖图。

---

## F17 — `ATTR_TOOL` / `ATTR_PROJECT_CONFIG` 与 env_hints 链路未闭合

**严重度：** MINOR  
**维度：** 归因规则 / 事实  
**设计章节：** §1.2、§1.4 规则 7

**证据：**

- 枚举含 `ATTR_TOOL`、`ATTR_PROJECT_CONFIG`；规则表无 TOOL；规则 7 → PROJECT_CONFIG。  
- 现网 `env_error` 强依赖 `env_hints`（`TaskResult.env_hints`，DESIGN §3.5「必须说清缺什么」）。  
- 提案 remediation 变成人话 `repeated string`，未规定 `env_hints` 字段存留与 PROJECT_CONFIG 的映射；旧 agent 只读 `env_hints` 会空。

**建议修改：**

1. 规定：`PROJECT_CONFIG` 必须继续填充 `env_hints`（或显式废弃 + 版本协商）。  
2. `ATTR_TOOL` 删除或给出唯一产生规则（避免死枚举）。

---

## F18 — Notice identity = `category ‖ text` 导致主机列表抖动刷屏或漏报

**严重度：** MINOR  
**维度：** notices 生命周期  
**设计章节：** §3.2

**证据：**

- pending egress 文本内嵌 host 列表（`engine.rs:906-913`）。host 集合增删 → identity 变 → 全文再报（合理）；集合相同但排序不稳 → 假变化。  
- 反过来说：只把 category 当 key 会在「从 3 个 pending 变 1 个」时静默。

**建议修改：**

1. identity 用结构化字段：`blake3(category ‖ canonical_sorted_hosts)`。  
2. 状态机表增加「集合变小/变大」的强制全文策略。

---

## F19 — 规则顺序表未编码 `timed_out` 与现网「超时压倒一切」等价物

**严重度：** MINOR  
**维度：** 归因规则 / 超时  
**设计章节：** §1.4，对照 `diag.rs:245-247`、`timeouts_win_over_everything`

**证据：**

- 现网：`timed_out` 最先返回 Timeout，无视诊断。  
- 提案规则 1：`timed_out && worker_killed`。若超时路径只设 `timed_out` 而漏设 `worker_killed`，规则 1 不中；若此时日志有 error 诊断，可能落到规则 8 CODE——**超时被定罪为代码问题**。  
- docker 超时路径确会 kill（`docker.rs:356-361`），但分类器输入必须 **强制** `worker_killed=true` 的写入点写进设计，否则实现漏写即回归。

**建议修改：**

1. 规则 1 改为 `timed_out`（worker 证据缺失时仍 Timeout/UNKNOWN-timeout），`worker_killed` 仅作证据字段。  
2. 保持单测：`timed_out` 压倒诊断。

---

## F20 — shell 前缀 env「碰巧正确」论断需限定；command 覆盖路径

**严重度：** NIT  
**维度：** 事实 / env  
**设计章节：** §2.4

**证据：**

- fingerprint 含 **整行 command**（`profile.rs:232`）。`ENV=VAL cargo test` 前缀会变 command → 破缓存，说法成立。  
- 但 `profile.env` 不经 shell 前缀、经 worker `cache_config` 注入，已在 canonical 中——并非全靠 command hack。  
- MCP `command` 覆盖（`mcp.rs:387`）与未来 `env` map 双通道：谁覆盖谁未写。

**建议修改：**

1. 收窄措辞：仅「请求级 shell 前缀」是 hack。  
2. 写明 `command` 与 `env` 同时出现时的合并与指纹字段。

---

## F21 — 请求 env 的指纹方案与服务端权威重算互相冲突

**严重度：** BLOCKER  
**维度：** 任务契约 / env 分层 / fingerprint / 缓存正确性  
**设计章节：** §2.4、§6

**证据：**

- 提案给 `SubmitTaskReq` 单独增加 `env`，并说「请求 env 追加进哈希输入」。  
- 现网服务端不信任客户端 fingerprint：`app.rs:202-211` 用 `manifest.root_hash + ResolvedProfile.canonical` 重新计算，客户端传来的不同值只记 warning 后丢弃。  
- worker 实际只收到 `TaskAssignment.profile`，`runner.rs:319-327` 仅从 adapter `cache_config(profile)` 生成容器 env；单独的请求 env 若没有先合并回 `ResolvedProfile.env`，既不会执行，也不会进入 server fingerprint。  
- 因而存在两个都错误的直觉实现：
  1. agent 把 request env 只追加到自己算的 hash：server 丢弃该 hash，不同请求仍可命中同一缓存；
  2. server 只把 request env 传给 worker：执行不同、canonical 相同，直接缓存投毒。
- `EXECUTOR_ABI` 只能隔离一次全局语义升级，不能区分同一版本中两个不同 request env。

**建议修改：**

1. server 验证请求 env 后，合并为一个 **effective resolved profile**，重新生成 canonical，再计算 fingerprint，并把同一 effective profile 下发 worker；禁止维护「hash 一份、执行 env 另一份」。  
2. agent 本地也必须用同一共享 canonicalizer，否则本地 `ResultCache` 会与服务端键不一致；最好由 rc-core 暴露唯一的 `resolve_env + canonicalize`。  
3. 加端到端测试：同 manifest/profile、仅 request env 不同，服务端 fingerprint 不同且 worker 所见 env 分别正确。

---

## F22 — `raw=true` 与 8KB 总响应上限在按行分页下不可同时满足

**严重度：** MAJOR  
**维度：** 预算门 / 单行省略 / 8KB 响应  
**设计章节：** §3.1

**证据：**

- 提案要求所有 MCP 响应 `≤8192 bytes`，又允许 `get_log(raw=true)` 跳过单行省略；现有 `LogQuery.offset/limit` 是**行**分页（`rc.proto:199-205`），没有行内 byte offset。  
- 一条 100KB linker 命令即使 `limit=1`，raw 模式若完整返回就违反 8KB；若 BudgetGate 再截断，下一页仍从下一行开始，当前行被截掉的 92KB 永远取不到。  
- 中间省略也不是普遍安全：缺失的 `-lfoo`、`--extern` 冲突项、宏展开路径、backtrace 中段和 JSON 单行字段都可能正好位于中间；「定位信息在头尾」不是可依赖的不变量。  
- `max_line_bytes` 名为 bytes，但设计又写保留「字符」；UTF-8 中文/路径若按 byte slice 可能切坏编码，按 char 计又可能超过 byte 上限。

**建议修改：**

1. 二选一：raw 仍受单响应限制并增加 `line_byte_offset/next_byte_offset`；或明确 raw 是独立下载/资源句柄，不直接塞 MCP 文本。  
2. 普通模式优先做格式感知压缩（识别 rustc JSON/linker argv），中间省略只作最后兜底，并返回原始行号、原始长度和稳定 hash。  
3. 预算算法统一按 UTF-8 bytes 计数、按 char boundary 截断；测试覆盖单个超大 Unicode 行。

---

## F23 — Notice 的“进程生命周期”不是会话语义，会跨项目漏报且不会识别消失后复发

**严重度：** MAJOR  
**维度：** Notice 状态机 / 会话生命周期  
**设计章节：** §3.2

**证据：**

- 状态键仅 `HashMap<category,last_identity>`，未含 `project_id`、`worktree_id`、调用方/对话 session。一个常驻 rc-agent 处理多个项目时，项目 B 与 A 恰好相同的 exclude 文本会被当成“已说过”；不同项目交替调用又会让 identity 来回变化、反复全文输出。  
- 生产者只在 notice 存在时产出对象；状态机没有处理“本次 category 缺席”。因此 `exclude` 出现 → 消失 → 以同样文本复发时，旧 identity 仍在，复发会被静默，恰恰漏掉状态变化。  
- MCP 进程重启后遗忘本身是**安全偏置**：最多重复一次，不会漏告警；可接受。真正不可接受的是把进程当 conversation，以及无 TTL/无 clear transition。  
- Critical 重复压成「同前」仍假设调用者能看到“前一次”：多个 LLM client/对话共享进程时不成立。

**建议修改：**

1. key 至少为 `(agent_session/conversation_scope, project_id, worktree_id, category)`；若拿不到 conversation id，就不要静默 correctness-critical notices。  
2. 每次调用提交完整 notice snapshot，显式记录 present/absent；消失后复发必须视为首次。  
3. Info 可用短 TTL 去重；exclude、baseline-off、egress-refused 等影响结果解释的 notice 每次保留一行自包含摘要。无需为跨进程持久化增加数据库。

---

## F24 — `TestOutcome.parse_ok=false` 不能证明“未进入测试阶段”

**严重度：** MAJOR  
**维度：** 任务契约 / ATTR_CODE 规则 9 / 自动呈现  
**设计章节：** §2.2–§2.3、§1.4

**证据：**

- 设计把 task 类型直接绑定 `TestContract`，但现网允许 profile 覆盖 `tasks.test`（`adapter.rs:140-146`），DESIGN 示例就是 `cargo nextest run`；它未必输出 libtest 摘要。`harness=false` 测试、`cargo test --no-run`、自定义脚本也同样不匹配 regex。  
- 测试二进制可能已经运行后 abort/SIGKILL，因而没有摘要；`parse_ok=false` 只能证明“未识别到摘要”，不能证明“未进入测试阶段”。把后者当归因证据违反 I1。  
- 多个 test binary 的 `failures:` 块中测试名未附二进制身份；同名测试会碰撞，输出交错/自定义 formatter 也会让块解析漂移。  
- 设计称 libtest 格式稳定，但没有为 nextest/custom 命令定义 capability negotiation 或 fallback。

**建议修改：**

1. 字段改名为 `summary_seen`/`outcome_parse_status`；呈现为「未识别到测试摘要」，不得推断阶段。  
2. 只有默认 cargo/libtest 命令启用该 parser；自定义 test 命令显式选择 parser，默认 `CustomOutcome(exit_code)`。  
3. failed name 用 `(binary/package, test_name)`；fixtures 覆盖 nextest、doctest、harness=false、abort-before-summary 和多个 binary。

---

## F25 — normalize 删除数字会抹掉类型语义，不只是行列漂移

**严重度：** MAJOR  
**维度：** 诊断 delta / 身份碰撞与漂移  
**设计章节：** §4.2

**证据：**

- 设计说剥去消息内嵌的“行号/列号数字”，但没有语法位置识别；若实现为通用数字删除，会把 Rust 诊断中的语义值一并删除，例如数组长度 `[u8; 32]` vs `[u8; 64]`、tuple index、常量泛型、端口/版本和 “expected 2 arguments”。  
- `file_path` 全量入键使 rename/move 必然表现为 fixed+new；宏展开/生成代码路径在不同 worker 根路径下也可能漂移。  
- 仅按同键出现次数配对无法判断两个相同诊断中到底哪一个移动/修复；用展示行号“消歧”并没有参与匹配算法。

**建议修改：**

1. 不做通用数字剥除；只规范化已由结构化 span 提供、可精确识别的 `file:line:column` 片段，保留其余数字。  
2. 采用两级匹配：严格键（level/code/message/path）计算可靠 delta；可选 fuzzy 键只做“疑似移动”提示，不计入确定 new/fixed。  
3. rename、generated path、空 code、截断任一出现时置 approximate，文案禁止「非本次改动引入」这种因果断言。

---

## F26 — `Compiling/Checking` 是开始事件，不是 `units_done`；历史总量也不是 ETA

**严重度：** MAJOR  
**维度：** 流式 / units_total / ETA / 日志落盘风险  
**设计章节：** §5.2–§5.3

**证据：**

- Cargo 输出 `Compiling foo`/`Checking foo` 表示单元开始，不表示完成；将每个匹配行累加到 `units_done` 会在慢 crate 刚开始时宣称已完成，并可能因同 package 多 target/build-script 重复计数。  
- `Fresh` 默认非 verbose 输出未必出现；把 Fresh 当稳定总量来源没有协议保证。Cargo 人话输出也不是机器稳定接口。  
- 上次任务的单元数受 features、targets、Cargo.lock、增量/target volume、sccache 与命令覆盖影响；即使“同 profile”，树变化也会改变图。展示 `23/107` 伪装成精确进度，可能超过 100% 或长期停在低值。  
- `build_ms` 历史 p50 是**总历时分布**，不是当前任务剩余 ETA；worker 硬件、冷热缓存和队列状态不同会使其不可比。  
- 现有 docker 已持续 drain `logs(follow=true)`；若新实现为每行 `await` 发送 progress，worker→server channel 背压会阻塞日志 drain。若同时每 2 秒把事件写 SQLite `task_events`，还会把高频瞬时状态永久化并增加单控制面写锁竞争。

**建议修改：**

1. 字段改为 `units_started/units_seen`，不要叫 done；首版只显示 `正在处理 foo（已观察 N 个单元）`。  
2. 历史 p50 标为“历史总耗时参考”，不提供 ETA/剩余时间；按 project/task/profile/冷热状态分桶前不要显示确定分母。  
3. 日志 collector 与 progress emitter 解耦：collector 永不等待控制面，使用有界 latest-value channel/coalescing；进度默认只存最新值或低频采样，完整日志 accumulator/spool 单独保持。

---

## 总评

提案把真实痛点（OOM→`compile_error`、test 无结局 schema、日志灌上下文、样板 notice、无增量诊断、无进度）收成机制层，方向对准 DESIGN §1「Token 经济 + 正确归因」。但作为**可实施设计**，当前文本在四处不可越过的断裂：

1. **旧 `ResultKind` 宇宙未闭合**（缓存、重试、hint、metrics、DESIGN §3.5 磁盘满）；  
2. **规则表与 PR 序使 test 归因先坏后好**；  
3. **profile/基线身份在 schema 中不存在**；  
4. **请求 env 与服务端权威 fingerprint 不闭合**；  
5. **RESOURCE 自动补救与 cancel/progress 控制面路径写错或过粗**。

在这些闭合前，按文档实现会引入：错误缓存、错误重试、错误 delta、取消无效、以及比「信息不足」更贵的 **错误确信**——直接打脸 §0 自写原则。

对照 DESIGN §1.1.5，应先交付 **可证伪的 OOM/137 分判 + 不可缓存**，再叠加 TestOutcome 与预算门；不宜一次五层全上。

---

## 最需改的三处

1. **补全 `Verdict → kind/cacheable/retryable/hint` 与磁盘满策略**（F1、F3、F12 缓存）——否则任何归因改造在现网管道上行为未定义。  
2. **重写规则表落地顺序与 ATTR_CODE 产生条件**（F2、F8、F9、F19）——保证 PR1 不回退 test 失败，且 raw-log env/SIGKILL 不会覆盖真编译错误/超时。  
3. **统一 effective env/profile/fingerprint，并定义基线与事件的真实键**（F4、F5、F14、F21）——否则机制二/四/五会缓存错结果、串分支或无法兼容长轮询。

---

## 核实矩阵（点名代码）

| 设计声称 | 核实结果 |
|---------|---------|
| `classify` 默认 test→CompileError（diag ~314） | **属实**（311–320）；OOM 无诊断时走此路 |
| docker 仅 worker 杀时认识 137（355–390） | **属实**；不读 OOMKilled |
| test 命令无 json（adapter:163） | **属实** |
| cache_config 无 TEST_DEBUG=0（181–201） | **属实** |
| format_result 按 kind（1010–1086） | **属实** |
| describe_* 每次拼接（866–955）；exclude 故意每次 | **属实**（879–881） |
| result_json 存 tasks（store 690–719） | **属实** |
| fingerprint 索引 + compute 含 profile_canonical | **属实**（schema:146；fingerprint 78–100；canonical 含 env） |
| docker 335–353「一次性收完」 | **不属实**；已是 follow 流式入缓冲 |
| cancel 复用 supersede 379–420 | **不属实**；supersede 不管 running；admin cancel 另路径 |
| 同 profile 基线/ETA | **schema 无 profile_id，不可直接实施** |
