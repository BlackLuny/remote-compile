# intent-and-query-surface 设计复核 r2

评审者: kimi k3
日期: 2026-07-28
文档版本: 草案 v2

## 总判

- 裁决: **APPROVE**
- 一句话理由: 第一轮 17 条全部闭合——四条 BLOCKER 的闭合（§3.4 wire 契约、§3.7 scope 键、§3.8 publish 规则）不仅写了原则还写了推导序、迁移窗口与机械化验收，达到可开工标准；残余为 1 MINOR + 2 NIT 的新发现，均不阻断。
- 第一轮 BLOCKER 闭合状态表:

  | ID | 摘要 | 状态 |
  |----|------|------|
  | F1 | 默认命令不过 wire；server 无 PathContext 重算 | **CLOSED** |
  | F2 | worker parser 门控重算失败 | **CLOSED** |
  | F3 | publish 冻结 `-p` 为 fleet 默认 | **CLOSED** |
  | F5 | delta 跨 scope 假 fixed/new | **CLOSED** |

- 第一轮 MAJOR 闭合状态表:

  | ID | 摘要 | 状态 |
  |----|------|------|
  | F4 | supersede 无 scope 互杀 | **CLOSED** |
  | F6 | members 缺 exclude → 错匹配 | **CLOSED** |
  | F7 | suggest 90 vs server min(60) | **CLOSED** |
  | F8 | get_diagnostics 未定义实现/分页 | **CLOSED** |
  | F9 | workdir 悬空 / 指纹 | **CLOSED** |
  | F10 | 无 proto 挂点清单 | **CLOSED** |

- 新发现计数: 3（R2-F1 MINOR / R2-F2 NIT / R2-F3 NIT），全部不阻断开工。

## 逐条复核 F1–F17

### F1
- 状态: CLOSED
- 依据: §3.4「Wire 契约（F1+F2 闭合 — S1 前置）」；§0 新增设计原则 4；§2.3 新增回归哨 `agent_server_fp_match_rate`；§1.3 锚点表补全 wire 事实。
- 闭合要点核对:
  - `SubmitTaskReq` 必须携带 `path_context` + `command_is_default`（§3.4.1），「可选：纯解析」措辞显式删除（§3.4.6）；
  - server 推导序「显式 override > profile.tasks > adapter.default(path_context)」（§3.4.3），且明令「不得 `Default::default()` 空 profile」——顺带修掉了我第一轮在 F2 证据里指出的**现网** target/features 被 server 重算丢弃的潜在缺陷；
  - 指纹/canonical/TaskAssignment 同源（§3.4.4），不一致以 server 为准并记 metrics；
  - 端到端验收含「crateA 与 crateB 指纹不同」，正好覆盖我指出的跨 crate 服务端缓存串味情形（§3.4 验收第 2 条）；
  - 旧 agent 兼容路径写明（§9.1：不传 path_context → 与今日一致）。

### F2
- 状态: CLOSED
- 依据: §3.4.5「worker 不再重算默认命令字符串；`command_is_default` 由 assignment 下发布尔；TestContract parser 门控只读该布尔」；§9.1 `TaskAssignment.command_is_default`；§3.10 测试「scoped 默认 test → parser 启用、TestSummary 产出」；§9.1 旧 worker 回退说明（诚实标注「仅无 -p 时正确」）。

### F3
- 状态: CLOSED
- 依据: §3.8「`command_is_default == true` 不得物化进 `profile.tasks`」+ 测试「scoped success 后 get_build_profile 的 tasks 不含 `-p`」；§9.2 kill-switch 恢复旧行为；§11.2.5 契约测试。
- 备注（边界，非重开）: §3.8 仍允许 `command_is_default == false`（人写/显式命令，如手敲的 `cargo check -p zf-web`）进入 fleet tasks——手写的 `-p` 同样可能冻结 scope。但这是「人写覆盖 = 知识」的有意划界，且有 §3.3 规则 1 的 Critical scope_mismatch Notice 兜底，属于产品设计决策而非漏洞。

### F4
- 状态: CLOSED
- 依据: §3.7 supersede 键增加 `scope_hash`；§9.2 迁移窗口（旧 pending 无 hash 按「空 scope」或保守三元组，注明「升级窗口可能多杀一次」）；§11.2.4 契约测试「check(A) 排队中再 check(B) → A 不被 supersede」。
- 备注: 迁移策略给了「或」的两个选项，未拍死；两种后果都有界（多留一个旧 pending / 窗口内多杀一次），可接受，实现 PR 时选定其一即可。

### F5
- 状态: CLOSED
- 依据: §3.7 auto/last_success 基线均限同 scope、「无同 scope 历史 → 视同 none」、「跨 scope 不得报告 fixed_count / 把兄弟诊断当 new（F6.3 再现）」；§1.4 边界表同步；§9.2「无 scope_hash 的历史任务不参与 scoped auto 基线」；§11.2 测试。

### F6
- 状态: CLOSED
- 依据: §3.3 解析规则第 2 步「glob 展开后减去 exclude 模式（只作用于 glob，不过滤显式 member）」+ 第 5 步「自校验：解析出的 package name 必须仍在清单内，否则回退 WORKSPACE + resolve_note」——错匹配方向被自校验兜住；§3.10 fixture 含「glob + exclude 不把 excluded 目录当 member」。

### F7
- 状态: CLOSED
- 依据: §7.2 拍板 server 上限 60→120（`wait_secs.min(120)`），建议区间 60–120 与之一致；`suggest_wait_secs = clamp(p50*1.2, 15, 120)`；默认 default_wait_secs 仍 4 不堵 MCP；§9.2 迁移行 + §13 连接占用风险入风险表；§11.1 测试「server 接受 90 并等到 90」。
- 备注: 示例与公式有一处小不一致，见 R2-F2。

### F8
- 状态: CLOSED
- 依据: §4.3 写死实现路径（不新增 RPC，agent 侧 `get_task(wait=0, baseline=…)` + 本地过滤分页）；签名补 `baseline?` 参数；分页语义表四项（total=stored、truncated 展示、越界文案、only_new 无基线时明说「不装成 empty success」）全部落地。

### F9
- 状态: CLOSED
- 依据: §3.5 cwd 不变量「intent scope 只影响命令，永不改变容器 cwd；workdir 唯一由 profile.path 决定」；PathContext 消息删除 `workdir_rel`（字段 6 不再出现）；§2.2 非目标；§3.3「profile.path 唯一决定容器 workdir，MCP path 不覆盖，Receipt 可观测」。

### F10
- 状态: CLOSED
- 依据: §9.1 挂点表 14 行（消息/字段/号/生产方/消费方），含 `ProfileResp.pending_pre_commands`（repeated string，解决 skipped 列表数据源）与 `TaskStatus` 队列字段；号位经我抽查与现网 proto 不冲突（`SubmitTaskReq` 现最大号 15=env → 16/17 可用；`TaskResult` 止于 14 → 15 可用；`LogChunk` 止于 9 → 10/11 可用）；「号以落地为准、表约束语义挂点」的免责合理。§5.3 补了数据未就绪时的诚实降级（`pre_commands=unknown` 而非谎称 ran）。

### F11
- 状态: CLOSED
- 依据: §3.3 规则 1「→ Critical Notice `scope_mismatch`（identity = profile_command ‖ derived_packages）」；§1.4 边界表；§2.3 度量描述同步。

### F12
- 状态: CLOSED
- 依据: §5.2 headline 内嵌限定示例 `✓ zf-web: 15 warnings [成功, scope=package]`；§11.1 format_result 测试「headline 含 scope」。

### F13
- 状态: CLOSED
- 依据: §4.5 将渲染改写**降级为非目标**（采纳我建议的选项 B），并写明若后续做渲染的约束（grep 恒按 raw、行号恒 raw、1:1 替换、证据直达默认 raw=true）；§4.7 的 get_log footer 已带 `raw=true`。

### F14
- 状态: CLOSED
- 依据: §1.2 #10 改为「收敛为 Critical compact 一行（不追求沉默）」；§1.4 边界表；§12 runbook「exclude 每次 compact 一行是有意（正确性），不是 bug」；S5 验收口径同步。

### F15
- 状态: CLOSED
- 依据: §3.6 写明「manifest 全量覆盖 → scoped miss 是有意取舍，非回归；scoped 子树 manifest 列入后续」；§2.2 非目标；§2.3 注「秒回前提 = 同 scope 且 manifest 未变，勿与 scope 实现失败混淆」。

### F16
- 状态: CLOSED
- 依据: §3.1 示例改为「结果含无关 crate 的 E0063，被当成 path 的答案」并附 v1 误写勘误；§0 原则 1 同步改为「作用域过大导致结果错绑，不是 fingerprint cache 误命中」。

### F17
- 状态: CLOSED
- 依据: §3.3 序 2 命令形态补 `--all-targets`；§5.2 Receipt 示例 `cargo check -p zf-web --all-targets --message-format=json`；ScopeKind 删除 SCOPE_PATH 并注明「不预留死枚举，未来 additive 加号」。

## 新发现

### R2-F1: `command_is_default` 的权威方未钉死——agent 上报与 server 推导形成双源
- 严重度: MINOR
- 类别: 正确性 / 不变量冲突（与文档自己的原则 4 及上一轮 R2「不信 client canonical」的一致性）
- 问题: §3.4.1 说携带「`bool command_is_default`，**或**可从 ScopeKind 推导」；§9.1 挂点表该字段生产方写「agent（rc-core）」，消费方「server → assignment」。若 server 直接信任 agent 上报的布尔而不经 `resolve_command` 重算，则执行语义（worker parser 门控）的判据来自 client 单方——与同文档原则 4「执行语义与指纹/回执必须同源，server 权威重算必须消费 PathContext」、以及上一轮「server 重建 canonical、client lie 被忽略」的先例（contract.rs `effective_profile` / app.rs:1363 测试）不一致。布尔与 path_context.scope 不一致时（bug 或伪造），worker 门控会按 client 的说法开关 libtest 解析 → TestSummary 可被人为开关 → 归因规则 4 的硬证据链受 client 影响。
- 证据: §9.1 表「`SubmitTaskReq.command_is_default` | 17 | **agent（rc-core）** | server → assignment」；§3.4.1 的「或」；对照 §3.4.3 仅说命令的权威重算，未说布尔的权威重算。
- 建议: 一句话钉死：server 用唯一 `resolve_command(profile, task, path_context, command_override)` **重算** command_is_default（推导规则：ScopeKind ∈ {WORKSPACE, PACKAGE} 即 true），agent 上报值仅作交叉校验并计入 fp_match 类 metrics，与 canonical 同原则。实际后果轻微（agent 只能骗自己的归因），故定 MINOR。
- 若忽略: 实现者按 §9.1 字面取信 client 布尔，门控判据出现第二个事实源；与原则 4 的自洽性破一个小口，未来排查「TestSummary 为何消失」时多一条歧路。

### R2-F2: §7.2 suggest 公式与示例数值不一致（102 vs 90）
- 严重度: NIT
- 类别: 与文档自身事实
- 问题: §7.2 公式 `suggest_wait_secs = clamp(history_p50_ms/1000 * 1.2, 15, 120)`，`history_p50_ms=85000` 应得 102，而同节 pending 示例写 `suggest_wait_secs=90`。实现者抄示例还是抄公式会产生 12s 分歧（不涉正确性，但示例在此项目里有被照抄的前科——第一轮 F17）。
- 证据: §7.2 表格与 pending 响应示例并列于同一节。
- 建议: 把示例改为 `history_p50_ms=85000 suggest_wait_secs=102`，或公式改为无 1.2 系数。
- 若忽略: 无实害；哨兵度量比对时可能出现「为什么 suggest 对不上」的疑问。

### R2-F3: §9.1 新增 `history_p50_ms` 与现网 `history_build_ms_p50` 重复
- 严重度: NIT
- 类别: 协议 / API
- 问题: §9.1 挂点表列 `TaskStatus.history_p50_ms`（additive 新字段，server 生产）；但现网 TaskStatus 已有机制五落地的 `history_build_ms_p50 = 15`（proto:280，engine.rs render 已消费）。新增同义字段会造成双字段。
- 证据: `crates/rc-core/proto/rc.proto:279-280`（`history_units_p50 = 14`、`history_build_ms_p50 = 15`）vs §9.1 表。
- 建议: 表中该行改为「复用现网 `history_build_ms_p50`」，或在落地 PR 删除新字段提议；顺带 §11.1「拒绝 >120 截断」措辞宜改为「>120 截断到 120」（是 clamp 不是拒绝）。
- 若忽略: 实现期出现一个字段两个名字，旧 agent 读不到新字段时会显示「无历史」。

## 仍可开工的条件

- **S1 可以开工：是。** 第一轮的前置四闭合（wire 契约 §3.4、cwd 不变量 §3.5、scope 键 §3.7、publish 规则 §3.8）已全部入文档且互相一致；§3.10/§11.2 的机械化验收覆盖了四条 BLOCKER 的回归场景（fp 一致、跨 crate 指纹不同、scoped test 有 TestSummary、publish 无 `-p`、跨 scope 不互杀/不假 fixed）。开工前无剩余文档前置；建议把 R2-F1 的一句话（server 重算布尔）随 S1 一并实现——改动成本一句话，省去后续排查歧路。
- **S2 可以开工：是。** §4.3 实现路径写死为 agent 侧复用 `get_task`（现网已具备 diagnostics + delta 数据），仅 `LogChunk` 两个 additive 字段需要 server 配合，与 S1 无依赖冲突；§10 的「S2 可并行于 S1 后半」判断成立。
- S3–S5 无本轮新阻塞（S4 依赖的 wait 上限已在 §7.2 拍板；S5 的 `pending_pre_commands` 采集已入 §9.1）。

## 不建议再改的部分

- §3.4 的「唯一 `resolve_command` 入口 + server 权威 + fp 回归哨」三件套——第一轮 F1 的正解，勿在实现期退化为「agent 算好命令塞进 command_override」（那会把 scope=PACKAGE 错标成 EXPLICIT_COMMAND，并关掉 test parser）。
- §3.5 cwd 不变量的「永不改变」措辞——保持绝对，任何「特殊情况改一下」都会重演 abi2 墓碑。
- §3.8 以 `command_is_default` 划「机器推导 vs 人写知识」的 publish 界限——这是正确的切线，勿扩大为「一律不 publish tasks」（会破坏 fleet learning 对人写命令的沉淀）。
- §4.3 「不新增 server RPC、agent 侧过滤」的实现路径——零协议成本拿到 L1.5，是对的。
- §4.5 渲染降级为非目标、§7.2 上限 120 而非无上限、§3.7 scope_hash 单点规范化——三个「够用的最小决策」，勿再放大。
- §15 评审裁决记录与「保留不动」清单——如实反映了第一轮评审，保留。
