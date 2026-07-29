# intent-and-query-surface 设计评审

评审者: kimi k3 (design_review)
日期: 2026-07-28
文档版本: 草案 v1

## 总判

- 裁决: APPROVE_WITH_CHANGES
- 一句话理由: 机制划分与产品判断方向正确，但 S1 的核心——「intent path → 默认命令」——在现网 wire 链路上根本传不到执行端，且与 publish/supersede/delta 三个存量机制产生未声明的耦合，四处不闭合就不能开工；均为「写死契约」级修订，不需要推翻重设计。
- 计数: BLOCKER 4 / MAJOR 6 / MINOR 5 / NIT 2（共 17 条）

## 发现清单

### F1: agent 合成的默认命令不过 wire：server 权威重算会让 `-p` 根本不执行，Receipt 反而撒谎
- 严重度: BLOCKER
- 类别: 正确性 / 不变量冲突 + 与现网代码事实
- 位置: 文档 §3.3/§3.4/§5.2；代码 `crates/rc-server/src/app.rs:272-284`、`crates/rc-agent/src/engine.rs:341`、`crates/rc-core/src/profile.rs:255-271`
- 问题: 机制 A 假设 agent 本地算出 `-p <pkg>` 默认命令后它会成为执行命令。现网链路上这个假设不成立：
  1. agent 只把**显式** command 放进 `SubmitTaskReq.command_override`（engine.rs:341 `command_override: req.command.clone().unwrap_or_default()`）；
  2. `ResolvedProfile.to_pb()` 没有 command 字段（profile.rs:255-271）——agent 合成的默认命令不以任何形式进 wire；
  3. server 在 `command_override` 为空时**自行重算**：`profile.tasks.get(...).unwrap_or_else(|| command_for(&Default::default(), task_type))`（app.rs:272-281），连 profile 的 target/features 都丢弃（`Default::default()`），更没有 PathContext；
  4. server 用自己算出的 command 做 canonical、指纹和 `TaskAssignment`（app.rs:291-304、618），server 值与 agent 值不符时只 warn 一声「fingerprint mismatch; using the server-computed value」（app.rs:305-310）。
- 证据: 文档 §3.4 写「可选：纯解析，供 agent 在 submit 前本地完成」——与 app.rs:272-284 的 server 权威重算直接矛盾。
- 建议: 写死 wire 契约：`SubmitTaskReq` 新增 `PathContext path_context`（及 `bool command_is_default` 或 ScopeKind）；server 命令推导序改为「显式 override > profile tasks > adapter default(path_context)」，server 权威重算必须消费同一份 PathContext；删除 §3.4「可选：纯解析」的措辞，改为「必须随提交传输」。附端到端测试：scoped 提交的 server 指纹 == agent 指纹，且 worker 执行行含 `-p`。
- 若忽略: agent 算 `-p zf-web` → server 重算 `--workspace` → worker 实跑 workspace；Receipt（agent 侧渲染）却声称 `scope=package:zf-web`——本方案要消灭的 silent scope hijack 以更危险的方向复刻（声称窄、实际宽）。更糟：同 manifest 下 `check(path=crateA)` 与 `check(path=crateB)` 经 server 重算后命令相同 → canonical 相同 → **指纹相同 → B 直接命中 A 的服务端缓存**，即错误缓存命中。这是「不修就不能开 S1」的第一条。

### F2: worker 端 libtest parser 门控按「重算默认命令」比对，含 `-p` 的默认命令它算不出来
- 严重度: BLOCKER
- 类别: 正确性 / TaskContract parser 门控
- 位置: 文档 §3.4；代码 `crates/rc-worker/src/runner.rs:284-288`、`crates/rc-core/src/contract.rs:99-126`
- 问题: parser 启用门控是 worker 侧字符串比对：`command_is_default_resolved` 用 `for_task(task_type).default_command(&TaskFlags{profile: bp})` 重建默认命令再与 `assignment.command` 判等（contract.rs:124-125），而 `bp` 只从 ResolvedProfile 的 adapter/target/features 重建（contract.rs:110-123）——没有 PathContext。机制 A 让默认命令依赖 intent_path 后，worker 重建值必然 ≠ 实际命令 → `command_is_default=false` → `parse_outcome` 走 `CustomOutcome(exit_code)` → TestSummary 消失 → 归因规则 4（test_failed → CODE，要求硬证据）永不命中，失败测试退化为 UNKNOWN。
- 证据: 文档只写「`TaskContract::default_command` 同样接收 PathContext（或从 TaskFlags 携带）」，未提 worker 门控的重算路径；llm-contract-mechanisms.md §2.2 的门控设计（评审 F24）假定默认命令可凭 profile 重算，机制 A 打破了这个假定。（附带现网事实：server 用 `Default::default()` 重算命令（app.rs:279），profile 带 target/features 的 test 任务今天就已经过不了这个门控——机制 A 会把该缺陷从边缘案例变成主路径。）
- 建议: 门控判据从「worker 重算比对」改为「提交时由 rc-core 计算 `command_is_default` 布尔，随 SubmitTaskReq → TaskAssignment 下发」；worker 不再重算。契约测试：scoped 默认命令的 test 任务 `parsed=true`、TestSummary 仍产出。
- 若忽略: 子 crate 跑 test 失去 pass/fail 摘要与失败用例名，机制二的核心收益在 S1 主路径上被静默关掉，且失败测试被错误归因。

### F3: `publish_profile` 会把一次性 intent 的 `-p` 命令冻结成 fleet 默认
- 严重度: BLOCKER
- 类别: 正确性 + 语义变更风险
- 位置: 文档 §3.3 规则 1、§9「profile [tasks] 行为不变」；代码 `crates/rc-agent/src/engine.rs:444、699-733`
- 问题: 首次 success 且 server 无 profile 时，agent 把 `resolution.command` 原样写进 `profile.tasks[task_type]` 并 upsert（engine.rs:705-707、723-728）。S1 后该 command 含 `-p <恰好第一个跑绿的 crate>`。之后所有 agent 对该 project 的所有 check 命中 §3.3 规则 1（PROFILE_OVERRIDE 优先于 path 推导），scope 被永久劫持；查其他 crate 只剩一条 Notice。
- 证据: engine.rs:705-707 `profile.tasks.insert(resolution.task_type.as_str().to_string(), resolution.command.clone())`；§9 兼容表声称「profile [tasks] 行为不变」——在 publish 这条耦合未处理时不成立。
- 建议: publish 时若 `command_is_default`（命令由契约/intent 生成而非人写），**不物化进 tasks**（或物化为去 scope 的 workspace 形态）；只有显式定制的命令才进 fleet profile。写进 §3.3/§9，加测试：scoped success publish 的 profile 不含 `-p`。
- 若忽略: 新项目接入后第一个跑绿的人把全 fleet 的默认 scope 锁死在一个 crate，状态自我延续且表面看像「项目配置本就如此」——安全误判（以为查了想查的东西）。

### F4: supersede_key 不含 scope——不同 crate 的排队任务互相取消
- 严重度: MAJOR
- 类别: 正确性 / 与现网不变量冲突
- 位置: 文档 §3 未提及；代码 `crates/rc-server/src/app.rs:421`、`crates/rc-server/src/store.rs:589`、DESIGN.md §5.2
- 问题: supersede 键是 `ids::supersede_key(worktree_id, agent_session, task_type)`（app.rs:421），清 pending 按此键匹配（store.rs:589）。现网同 task_type 的命令恒为 `--workspace`，「新代码取代旧代码」语义成立；S1 后同 session 连续 `check(path=crateA)`、`check(path=crateB)` 是两个不同问题，B 仍会 supersede 排队中的 A。agent 轮询 A 得到 superseded，会误解为「代码又变了」。
- 证据: DESIGN.md §5.2 定义 supersede 作用域为 (worktree, agent_session, task_type)（当时默认命令无 scope 维度，该键足够）；文档 §9 迁移表未列 supersede 键迁移。
- 建议: supersede_key 增加 scope 维度（resolved scope/command 的短哈希）；写进 §9 迁移表与 kill-switch 说明；加测试：同 session 不同 scope 不互杀，同 scope 仍 supersede。
- 若忽略: 多 crate 并行工作时任务「神秘被取代」；agent 学会的应对（盲目重发）放大队列压力（与 I10 目标反向）。

### F5: DiagDelta 的 auto 基线跨 scope 借用 → 批量「已修复」/「新增」假象
- 严重度: BLOCKER
- 类别: 正确性 / 不变量冲突（I4 + 上一轮 F6.3 裁决）
- 位置: 文档 §1.4（只消费 delta，未提基线键）；代码 `crates/rc-server/src/store.rs:779-799`
- 问题: 机制四已上线，auto 基线 = 同 `(project_id, worktree_id, task_type)` 最近完成任务。S1 使同 task_type 的命令随 intent path 变化：`check -p A` 以 `--workspace` 结果为基线时，A 之外的诊断全部从当前集合消失 → `fixed_count` 暴涨、展示「已修复 N 条」——正是上一轮 F6.3 裁决「假已修复比不报更糟」明令禁止的展示；反向 `check --workspace` 以 `-p A` 为基线，全部兄弟 crate 错误算「新增」。S1 让 scope 变化成为默认路径，此场景高频触发。
- 证据: llm-contract-mechanisms.md §4.3 基线键定义；store.rs:779-787 `resolve_baseline(project_id, worktree_id, task_type, ...)` 的签名即证无 scope/command 维度；本文档 §1.4 只写了「`get_diagnostics(only_new=true)` 的数据源」，未声明机制 A 对基线键的变更。
- 建议: auto 基线键增加 scope（或 command）维度：仅在**同 scope** 历史内取基线，无同 scope 历史视同 `none`；写进 §1.4 边界表与 §11 契约测试（跨 scope 的 auto 基线不得报告 fixed/new）。
- 若忽略: 展示层错误归因成为主路径默认行为：agent 被告知「修好了」从未检查的东西，或「引入了」早已存在的错误——I4 直接失守。

### F6: member 解析不含 `[workspace].exclude`，会把被排除目录误判为 member 并生成必败命令
- 严重度: MAJOR
- 类别: 可行性
- 位置: 文档 §3.3「package 解析来源（确定性）」
- 问题: cargo 语义中 glob 展开须减去 `[workspace].exclude`（exclude 只影响 glob，不影响显式列出的 member）；另有 default-members、path dependency 自动成员等差异。只实现 members+glob 时，`members=["crates/*"]` 且 `exclude=["crates/legacy"]` 的仓库会把 legacy 误判为 member → `cargo check -p <legacy-pkg>` 报「package ID specification ... did not match」→ 任务失败且归因像配置问题——规则 4 自己定的「无法映射就回退 workspace，禁止猜测」兜不住这个**错误命中**方向（它防的是「无匹配」，防不了「错匹配」）。
- 证据: 文档解析来源仅写「读根 Cargo.toml 的 [workspace].members（含 glob 展开，与 cargo 一致的最小实现）+ 每个 member 的 package.name」，§3.3 序 2/4 与解析 1-4 条均无 exclude；§13 风险表只承认「glob 与 cargo 不完全一致 → 回退」，未识别错匹配情形。
- 建议: 解析规则写死：glob 展开后必须减去 exclude 模式；显式 members 不做 exclude 过滤；解析出的 package 在提交前对 member 清单自校验，校验不过 → WORKSPACE + `resolve_note`。fixture 增加「glob 命中但 exclude 排除」「nested member」「根 package + members」用例（§11.1 已列部分，补 exclude）。
- 若忽略: 带 exclude 的 monorepo 从「安全回退 workspace」退化为「必失败的错误命令」——比现状更糟，且失败归因会误导。

### F7: `suggest_wait_secs` 的建议区间与 server 60s 硬截断矛盾
- 严重度: MAJOR
- 类别: Token / agent 行为 + 与现网代码事实
- 位置: 文档 §7.2；代码 `crates/rc-server/src/grpc_agent.rs:277`
- 问题: 文档建议冷 monorepo `wait_secs=60–120`，示例 `next: get_result(task_id="…", wait_secs=90)`；90 会被 server 截为 60，agent 在 60s 拿到 pending 空转返回，比文档承诺多一轮。
- 证据: grpc_agent.rs:277 `let deadline = Duration::from_secs(req.wait_secs.min(60) as u64)` 与 §7.2 示例直接冲突。
- 建议: 二选一写死：server 上限 additive 放宽（需评估长轮询连接占用），或建议区间与 suggest 算法上限钉死 ≤60。文档与实现保持一致后才写 tool description。
- 若忽略: I10 的轮次收益打折；tool description 给出不可达的建议值，agent 学到错误预期。

### F8: `get_diagnostics` 的传输路径与分页闭合未定义
- 严重度: MAJOR
- 类别: 协议 / API
- 位置: 文档 §4.3；代码 `crates/rc-worker/src/runner.rs:298`、`crates/rc-server/src/app.rs:989-1050`
- 问题: 未定义四件事：
  1. **传输路径**：新 RPC，还是 agent 侧复用 `get_task`（TaskStatus.result 已带 diagnostics + diag_delta，可零新 RPC 实现）？
  2. **total 语义**：`showing 11..20 of 23` 的 23 是 stored 数还是含 truncated 的真实总数？后者无法分页到达。
  3. **越界行为**：offset 越过 stored 上限时按 I9 该说什么，文档无文案。
  4. **only_new 的 baseline**：only_new 依赖 delta，delta 需要 baseline 参数，但工具签名里没有 baseline——语义悬空。
- 证据: 文档 §4.3 只写「数据源：任务结果中已解析的 diagnostics[]（及 delta）；不重新扫 raw log」；现网 server 只存 ≤50 条（runner.rs:298 `top_diagnostics(&diagnostics, 50)`）+ `truncated_diagnostics` 计数，delta 由 `task_status_with_baseline` 按 baseline 参数现算（app.rs:989-1050）。
- 建议: 写死：实现 = agent 侧 `get_task(wait=0, baseline=auto)` + 本地过滤/分页；total = stored 数，展示附 `(+N truncated)`；offset 越界返回 `(no more stored diagnostics; +N truncated 未存，get_log raw 可查)`；`only_new=true` 显式绑定 baseline 语义。
- 若忽略: 实现者各自发明；分页元数据在 truncated 场景撒谎，I9 在新工具上再次失守。

### F9: intent path 是否改变容器 cwd 未写死；pre_commands 的 cwd 与指纹维度悬空
- 严重度: MAJOR
- 类别: 正确性 / 语义变更风险
- 位置: 文档 §3.2（`workdir_rel` 注释「可与 profile.path 合并」）、§3.3 profile.path 合并段；代码 `crates/rc-worker/src/runner.rs:413`、`crates/rc-core/src/profile.rs:228`
- 问题: PathContext.workdir_rel 引入后未写死：谁消费、何时改 cwd、经什么通道进指纹。若 intent path 改 cwd：pre_commands 里的相对路径脚本（`./scripts/gen.sh` 类）或生成物落点漂移；若 workdir_rel 不经 profile.path 表达，则不进 canonical → 同指纹不同 cwd → 缓存投毒。
- 证据: 现网 workdir 唯一由 `subproject_workdir(anchor_mount, profile.path)` 决定（runner.rs:413），且 `profile.path` 进 canonical（profile.rs:228 `push("path", ...)`）；fingerprint.rs:60-73 的 abi2 注释正是「布局语义变了而哈希输入没变」事故的墓碑。
- 建议: 写死一条不变量：**intent scope 只影响命令（-p），永不改变 cwd；workdir 仍唯一由 profile.path 决定**。未来若要 path-scope cwd，必须经 profile.path 进 canonical 并进 Receipt。`workdir_rel` 字段要么删除，要么标注「展示用，不得驱动 worker」。
- 若忽略: 实现者把 workdir_rel 接进 worker → pre_commands 在错误目录执行，或指纹漏维度——两类都是上一轮已埋过的坑。

### F10: proto 变更没有清单；Receipt 的 pre_commands skipped 数据在 wire 上不存在
- 严重度: MAJOR
- 类别: 协议 / API + 切片依赖
- 位置: 文档 §3.2/§5.2/§7.2/§9；代码 `crates/rc-server/src/grpc_agent.rs:408-424`、`crates/rc-core/proto/rc.proto:146-165,294-305`
- 问题: 文档散落定义了 PathContext / EffectivePlan / PreCommandsStatus / LogChunk 两个新字段 / pending 导航字段，但没有挂点清单（哪个消息、字段号、生产方/消费方）。两处数据在现网 wire 上不存在：
  1. `PreCommandsStatus.skipped_commands` 与 ×N 计数：pending 的具体命令列表 server 不出给 agent → §5.2 的 Receipt 示例 `pre_commands=skipped(pending_approval)×2` 无法实现（S5 只含糊说「含采集」）。
  2. `queue_depth/running/capacity/suggest_wait_secs`：TaskStatus 无这些字段，§7.2 的 pending 导航无载体。
- 证据: §9 兼容表只有一句「proto 全部 additive」；现网 get_profile 对 pending 只返回一句派生文案（grpc_agent.rs:408-424，`pending_pre_commands` 是 bool）；queue 计数现仅存在于 ListWorkersResp（grpc_agent.rs:566-572）。
- 建议: 文档补一节 proto 变更表：`SubmitTaskReq.path_context`、`TaskResult.effective_plan = 15`、`TaskStatus.queue_depth/running/capacity/suggest_wait_secs`、`LogChunk.matched_lines = 10 / empty_reason = 11`、`ProfileResp.pending_pre_commands`（repeated string）；每行注明生产方与消费方。
- 若忽略: S1/S4/S5 开工即撞 wire 缺口，各切片各自发明挂点，评审过的设计在实现期漂移。

### F11: `scope_mismatch` Notice 严重度未定——按现转移表第二次就沉默
- 严重度: MINOR
- 类别: Token / agent 行为 + 安全
- 位置: 文档 §3.3 规则 1；上一轮 llm-contract-mechanisms.md §3.2；代码 `crates/rc-agent/src/engine.rs:1280-1286`（exclude 为 Critical 的先例）
- 问题: 规则 1 只说「不一致 → `scope_mismatch` 警告（Notice）」，未定严重度。scope 错配是「影响结果解释」的信息（正是 Critical 的定义）；若实现者按「警告」字面定为 Warning，第二次调用起 agent 在持续错 scope 中裸奔。
- 证据: 上一轮 Notice 状态机转移表：Info/Warning 在 identity 不变的重复时**沉默**，Critical 每次必显 compact（llm-contract-mechanisms.md §3.2）。
- 建议: 文档写死 scope_mismatch = Critical（每次必显 compact 一行），identity 含 (profile_command, derived_package)。
- 若忽略: F3 类事故或常态 profile/path 错配从第二次调用起不可见。

### F12: 成功 headline 不带 scope——「全绿」错觉的最后 10%
- 严重度: MINOR
- 类别: 安全
- 位置: 文档 §5.2 示例、§13 风险表
- 问题: scope 信息在 Receipt 第二行，「✓」的心理暗示集中在 headline；扫读漏掉第二行时，package 绿被当 workspace 绿。
- 证据: §5.2 示例 headline 为 `✓ 15 warnings [成功]`，scope=package 在次行；§13 风险表自认「自动 -p 漏掉 workspace 级 feature 统一检查」。
- 建议: scope=package 时 headline 内嵌限定（`✓ zf-web: 15 warnings [成功, scope=package]`），一行文案的成本。
- 若忽略: 缓解（Receipt 必显）仍在，仅加深；不阻断。

### F13: §4.5 的 get_log 渲染改写与 grep/行号语义交互未写
- 严重度: MINOR
- 类别: 协议 / API + Token / agent 行为
- 位置: 文档 §4.5；代码 `crates/rc-server/src/app.rs:923-931`
- 问题: §4.5 让非 raw 模式把 cargo JSON 行渲染成单行 summary。渲染后：grep="error" 命中的 raw 行其 summary 未必含「error」字样（过滤口径与展示口径分裂）；`get_log(offset=<evidence.line_no>)` 直达看到的是 summary 而非原文。文档未声明三者关系。
- 证据: 现网 grep 在 server 对 **raw 行**过滤（app.rs:923-931 `lines.iter().filter(...)`），offset/next_offset/机制一 Evidence.line_no 也都是 raw 行序号。
- 建议: 写死「grep 恒按 raw 行匹配；渲染仅 1:1 替换展示内容，行号/分页语义不变；证据直达场景默认 raw=true」。或把 §4.5 整体降级为非目标——footer 改指 get_diagnostics 后，get_log 的主诉（panic 栈、链接器全文）多为非 JSON 行，渲染收益有限。
- 若忽略: 同一个 get_log 三种口径（过滤按 raw、显示按 summary、行号按 raw），agent 无法形成稳定心智模型，分页错位投诉。

### F14: #10（exclude 警告刷屏）与上一轮 Critical 裁决的冲突被「落地即可」掩盖
- 严重度: MINOR
- 类别: 遗漏 / 与上一轮机制边界
- 位置: 文档 §1.2 #10、附录 A；上一轮 llm-contract-mechanisms.md §3.2；代码 `crates/rc-agent/src/engine.rs:1280-1286`
- 问题: #10 的痛点是「长期配置下脱敏失效」，文档承诺「已有 Notice 状态机，落地即可」。但 exclude 是 Critical，上一轮裁决 Critical **每次必显 compact**（F13.2：正确性关键信息不赌调用方记性）——落地后仍每次刷一行，#10 的「脱敏」诉求按设计**不会被满足**。文档把「有意保留」写成了「顺手修好」。
- 证据: engine.rs:1280-1286 exclude Notice 以 `NoticeSeverity::Critical` 创建；llm-contract-mechanisms.md §3.2 转移表「identity 不变的重复 → Critical 每次必显 compact 一行」。
- 建议: 改写 #10 的处理承诺：「收敛为每次一行 compact，不追求沉默（正确性关键，沿上轮裁决）」；附录 A 与 §10 S5 验收口径同步。
- 若忽略: S5 验收出现「#10 未解决」的假警报；更坏的是有人为迎合 #10 把 exclude 降级为 Warning，破坏 Critical 不变量。

### F15: 指纹仍含全 workspace manifest——兄弟改动击沉 `-p` 缓存的取舍未写明
- 严重度: MINOR
- 类别: 正确性 / 缓存 + 遗漏
- 位置: 文档 §3.4；代码 `crates/rc-core/src/fingerprint.rs:14-15`
- 问题: `manifest_root_hash` 覆盖全量同步内容，`-p` scope 下兄弟 crate 的改动仍改变指纹 → scoped check 缓存 miss。现网 `--workspace` 同样如此，**非回归**；但机制 A 的主卖点是高频子 crate 迭代，§2.3 的 `avg_tools_per_fix_loop → 1–2` 与「cache hit 秒回」（§8.2）都隐含这个假设。
- 证据: §3.4 只写「scope 变化不得命中旧 workspace 结果」（command 进 canonical 已保证，profile.rs:232），反向未提；fingerprint.rs:14-15 `manifest_root_hash: blake3 over the full workspace manifest`。
- 建议: 文档写明取舍：接受（依据 DESIGN §5.1 宁可多编译），scoped 子树 manifest 列为后续方向；§2.3 注明度量假设。
- 若忽略: zfc 主路径 cache hit 率低于预期，「秒回」不兑现，事后被当 bug 提回来。

### F16: §3.1 的「cache hit 到无关 crate 的 E0063」措辞不准
- 严重度: NIT
- 类别: 与现网代码事实
- 位置: 文档 §1.2 #1、§3.1
- 问题: 真实机制是「命令作用域过大导致结果包含无关错误，并被当成 path 的答案」，不是 cache hit。
- 证据: agent 改了 shared-crate 后 manifest 变化 → 指纹必变（fingerprint.rs:14-15 + profile.rs:232）→ 是新跑出的全量结果错绑归因；cache hit 只在 manifest 未变时发生，此时错误本来就是同一批。
- 建议: §3.1 示例改写为准确机制；不影响方案本身。
- 若忽略: 无实害；但评审/实施者对病根的模型会偏（以为要修缓存，实际要修 scope）。

### F17: Receipt 示例命令丢失 `--all-targets`；SCOPE_PATH 是死枚举
- 严重度: NIT
- 类别: 协议 / API
- 位置: 文档 §5.2 示例、§3.2 ScopeKind；代码 `crates/rc-core/src/adapter.rs:160`
- 问题: 示例命令不含 `--all-targets`，而现网 check 默认含它；§3.3 序 2 只说「test/clippy 同理加各自 flags」，check 自己的 flags 在示例里丢了——实现者抄示例会静默缩小检查覆盖面（test/bench target 不再检查）。另：`SCOPE_PATH = 3` 注明「未来 adapter 用」，是死枚举。
- 证据: §5.2 示例 `command=cargo check -p zf-web --message-format=json` vs adapter.rs:160 `cargo check --workspace --all-targets --message-format=json`；上一轮 F17 刚以同样理由删除死枚举 ATTR_TOOL（llm-contract-mechanisms.md §1.2 注）。
- 建议: 示例补齐 `--all-targets`；SCOPE_PATH 删除或在文中注明保留号位理由。
- 若忽略: --all-targets 静默丢失是又一次未声明的语义变更。

## 必须补进文档的闭合项

1. **wire 契约（F1+F2）**：`SubmitTaskReq` 携带 `PathContext` 与 `command_is_default`；server 命令推导序「显式 override > profile tasks > adapter default(path_context)」；worker **不再重算**默认命令，parser 门控用下发的布尔。
2. **publish 规则（F3）**：`command_is_default` 的命令不得物化进 fleet `profile.tasks`。
3. **supersede 键迁移（F4）**：`supersede_key := (worktree, agent_session, task_type, scope_hash)`；旧任务兼容说明。
4. **delta 基线键（F5）**：auto 基线限同 scope；跨 scope 视同 none；fixed/new 不得跨 scope 报告。
5. **proto 变更清单（F10）**：挂点 + 字段号 + 生产/消费方，含 `ProfileResp.pending_pre_commands` 与 `TaskStatus` 队列字段。
6. **member 解析规则（F6）**：exclude 过滤（仅作用于 glob）、显式 members 例外、package 名自校验回退。
7. **cwd 不变量（F9）**：intent scope 永不改 workdir；workdir 唯一经 profile.path 进 canonical 与 Receipt。
8. **get_diagnostics 定义（F8）**：实现路径（复用 get_task）、total=stored、truncated 呈现、越界文案、only_new×baseline 绑定。
9. **wait 上限拍板（F7）**：server 放宽或建议值 ≤60，二选一。
10. **scope_mismatch = Critical（F11）**：identity 组成写明。
11. **测试补强**：scoped success publish 不含 `-p`；跨 scope supersede 不互杀；跨 scope delta 无 fixed/new；scoped test parser 仍启用；glob+exclude fixture。

## 建议的文档修订优先级

1. F1 + F2 + F3 + F5 —— S1 前置闭合四件事（wire 契约、parser 门控下发、publish 规则、delta 基线键）；不修则 S1 产出错误缓存与错误归因。
2. F9 cwd 不变量 —— 一句话写死，低成本防指纹/执行两类事故。
3. F6 member 解析规则 —— fixture 先行，防「错匹配」方向的命令生成。
4. F10 + F8 —— proto 清单与 get_diagnostics 定义，S2/S4/S5 的开工图纸。
5. F7 wait 上限 —— 一次拍板，避免文档与实现各说各话。
6. F11–F15 —— Notice 严重度、headline scope、get_log 口径、#10 承诺、缓存取舍，随对应切片修订。
7. F16–F17 —— 文字与示例勘误。

## 不建议改动的部分

- §3.3 规则 0/4 与「无法映射就回退 workspace、禁止猜测 package 名」——安全偏置方向正确，错的是覆盖不全（F6），不是这条原则。
- profile.path 与 MCP path 冲突时「MCP path 赢 + Receipt 写明 ignored」——可观测性处理正确。
- Receipt 进 `TaskResult` 并随 cache 回放（§5.3）——与现网 `record_cache_hit` 整体复制 result_json（store.rs:830-836）的机制天然兼容，设计对了。
- LogChunk `matched_lines`/`empty_reason` additive 方案（§4.4）——与现网 server grep 后 `total=filtered.len()`（app.rs:932）的事实精确对接。
- resolve(ref) 禁止 silent pick、歧义返回 candidates（§6.2）。
- get_log 工具描述去掉 `grep="error"` 主路径推荐、失败 footer 改指 `get_diagnostics` 并直达证据行（§4.4/§4.7）——产品判断正确，是本轮最该保留的决策。
- kill-switch 按机制独立（§9）——与现网 `AgentConfig` 开关模式（config.rs:30-40）一致。
- 机制 E 不强制改默认行为、只在响应侧给建议（§7.2）——保住「不堵死 MCP」的底线。
- §3.3 用 TOML 解析而非默认 shell-out `cargo metadata`（§13）——性能与离线判断正确，metadata 留作 opt-in。
