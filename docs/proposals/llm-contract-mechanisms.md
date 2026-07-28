# LLM 契约化改造：五个机制层

> 状态：v2（对抗式评审修订版；评审 26 条发现的裁决见 §9，评审全文在
> `.omc/research/llm-contract-design-review.md`）
> 日期：2026-07-28
> 背景：一次真实使用会话（8 次调用：3 check + 5 test）暴露的七条反馈。本文档拒绝逐条打补丁，
> 把七条反馈归纳为五个缺失的通用机制层，给出各层的完整设计。

## 0. 产品契约与不变量

remote-compile 的消费者是 LLM agent。它的真实契约是一句话：

> **用最少的 token、以正确的归因、回答调用者此刻的问题。**

本次改造引入五条可测试的不变量，每条对应一个机制层：

| # | 不变量 | 机制 | 覆盖反馈 |
|---|--------|------|----------|
| I1 | 无证据不定罪：任何"该谁行动"的结论必须引用证据；无证据时输出"未知"而非猜测 | 机制一 证据化归因 | #1 |
| I2 | 结论回答任务的问题：每种 task 有自己的结局 schema | 机制二 任务语义契约 | #2 #3 |
| I3 | 任何进入上下文的文本过预算门：单行/单响应有上限，超限走分页而非灌入 | 机制三 上下文预算门 | #4 |
| I4 | 只说新信息：通知按状态变化触发（正确性关键通知除外）；诊断默认报增量 | 机制三(消息) + 机制四(诊断) | #5 #7 |
| I5 | 长任务可观测、可预估、可取消 | 机制五 流式生命周期 | #6 |

设计原则（补充到 DESIGN.md §1.1）：**分类错误比信息不足更贵。** 归因分类器 precision-first：
召回不足退化为"未知 + 证据摘录"，永远不退化为"错误的确信"。

---

## 1. 机制一：证据化归因（Verdict v2）

### 1.1 现状病根

`classify`（crates/rc-core/src/diag.rs）是 exit code + 有无结构化诊断的模式匹配链，
默认分支（diag.rs:311-320）把 "test 任务 + 非零退出 + 无诊断" 定为 CompileError（"代码问题：
按结构化诊断修改源码"，model.rs:127）。OOM 恰好走这条路：

- 内核 OOM kill 容器内进程 → cargo 报 `(signal: 9, SIGKILL: kill)`，容器非零退出；
- worker（crates/rc-worker/src/docker.rs:355-390）只在自己主动杀容器时认识 137，
  从不读 docker inspect 的 `OOMKilled`；全仓库没有任何地方检查 `signal: 9`；
- 于是 OOM 被报成"代码问题"，把调用方送去查一个不存在的编译错误。

病根不是某条规则写错，而是**分类器在没有证据时选择定罪而不是存疑**，且**证据采集不完整**。

### 1.2 数据模型：verdict 四元组

把 `ResultKind`（混合"结果状态"和"归因"两轴）拆成正交字段：

```protobuf
// rc.proto 新增（additive，不动现有字段）
message Verdict {
  Status status = 1;            // SUCCESS / FAILED / TIMEOUT / CANCELED
  Attribution attribution = 2;  // 谁该行动
  Evidence evidence = 3;        // 支撑归因的证据
  repeated string remediation = 4; // 建议动作（人话，一条一句）
  string rule = 5;              // 命中的规则名（可观测性）
}

enum Attribution {
  ATTR_UNKNOWN = 0;        // 默认值即"存疑"
  ATTR_CODE = 1;           // 代码作者：改源码
  ATTR_PROJECT_CONFIG = 2; // 项目配置：改 .remote-compile.toml / 依赖声明
  ATTR_RESOURCE = 3;       // 任务自身资源需求超限：OOM 及疑似
  ATTR_INFRA = 4;          // 基础设施：worker/网络/镜像/磁盘，用户无需动作
}

message Evidence {
  string source = 1;   // "docker_state" | "log_line" | "diagnostic" | "outcome"
  string excerpt = 2;  // 证据原文，≤400 字节（过机制三预算门）
  uint32 line_no = 3;  // source=log_line 时的日志行号，供 get_log(offset=) 直达
}
```

注：不设 `ATTR_TOOL`（无产生规则的死枚举，评审 F17）；预留枚举号 5 给未来。

### 1.3 旧世界闭合：verdict → 旧管道的完整映射（评审 F1）

`TaskResult.kind` 保留并由 verdict 推导。旧 kind 是全链路契约
（`store.find_cached_result` 的可缓存集合、`model.is_retryable` 的换机重试、
`app.finish` 的 metrics、`format_result`/`agent_hint`），映射必须逐格写死：

| status | attribution | 旧 kind | 可缓存 | server 换机重试 | agent_hint（新读者按 attribution 取词） |
|--------|-------------|---------|--------|-----------------|------------------------------------------|
| SUCCESS | — | success | 是 | 否 | 现状 |
| FAILED | CODE | compile_error | 是 | 否 | 按结构化诊断/失败测试修改源码 |
| FAILED | PROJECT_CONFIG | env_error | 否 | 否 | 现状（**继续填充 env_hints**，评审 F17） |
| FAILED | RESOURCE | env_error | 否 | 否 | 非代码问题；资源建议 + 自动补救声明 |
| FAILED | INFRA | infra_error | 否 | 是（现状路径） | 现状 |
| FAILED | UNKNOWN | env_error | 否 | 否 | 原因未知 + 证据摘录，建议 get_log |
| TIMEOUT | — | timeout | 否 | 否（现状） | 现状 |
| CANCELED | — | （不产生 TaskResult，见 §5.5） | — | — | — |

- **磁盘满归 INFRA**（与 DESIGN.md §3.5 一致：换机重试有效），不归 RESOURCE。
  RESOURCE 专指"任务自身内存需求超限"（换机通常无效、降配有效）。
- 可缓存集合不变：{success, compile_error}。RESOURCE/UNKNOWN 映到 env_error 即天然不可缓存。
- **agent 本地 `ResultCache.put` 增加同样的可缓存过滤**（现状无过滤，engine.rs:367 附近；
  评审 F12）——否则 OOM 结果会在本地缓存 24h。
- 新代码路径一律 switch on verdict；kind 仅供旧行与旧读者。

### 1.4 证据采集（worker 侧）

`RunOutput` 增加执行证据；`wait_container` 返回后、`remove_container` **之前** inspect 一次：

```protobuf
message ExecEvidence {
  bool oom_killed = 1;     // docker inspect .State.OOMKilled
  int32 term_signal = 2;   // 容器主进程终止信号（若可得）
  bool worker_killed = 3;  // 仅由 worker 在自己 kill（超时/取消）的代码路径置位
}
```

`worker_killed` 是证据字段，不是分类主键（评审 F19）：超时分类只看 `timed_out`。

### 1.5 分类器：有序规则表

`classify` 重写为有序规则表。输入 `Facts`：

```rust
pub struct Facts<'a> {
    pub task_type: TaskType,
    pub exit_code: i32,
    pub timed_out: bool,
    pub exec: &'a ExecEvidence,
    pub diagnostics: &'a [Diagnostic],
    pub test_summary: Option<&'a TestSummary>, // §2.3 的最小解析，PR1 就有（评审 F2）
    pub log_tail: &'a [&'a str],               // 最后 200 行
}
```

规则表（按序求值，先命中先赢）：

| 序 | 规则名 | 证据（匹配条件） | 归因 | 备注 |
|----|--------|------------------|------|------|
| 0 | success | exit_code == 0 | —(SUCCESS) | 现状语义不变（含 N warnings） |
| 1 | timeout | timed_out | INFRA(超时) | **压倒一切**，无视诊断（保持现网单测 `timeouts_win_over_everything`） |
| 2 | env_diagnostics | error 级诊断 ≥1 且**全部** `is_environment_diagnostic` | PROJECT_CONFIG | 现网精判原样迁移（diag.rs:168-176），继续走 env_hints |
| 3 | compile_error | error 级诊断 ≥1（存在非 env 诊断） | CODE | **现网不变量原样保留**：有真编译错误时，任何 raw-log marker 不得改判（评审 F9，单测 `a_compile_error_is_never_offered_a_package` 必须继续绿） |
| 4 | test_failed | test_summary 存在且 failed > 0 | CODE | 证据 = 摘要行 + 失败用例名 |
| 5 | oom_killed | exec.oom_killed == true | RESOURCE | 金证据；允许自动补救（§2.5） |
| 6 | sigkill_suspected_oom | 日志含 cargo 精确串 `(signal: 9, SIGKILL: kill)` | RESOURCE | "疑似 OOM"措辞；允许自动补救。理由：共享宿主机上内核全局 OOM killer 杀进程时 cgroup `OOMKilled` 可为 false（本项目生产环境正是共享宿主机）；重试一次成本有界，误归因成本无界。匹配串是 cargo 的固定格式化输出，不是宽松 `SIGKILL` 子串（评审 F8 的收紧要求） |
| 7 | rustc_crash | 日志含 rustc/cc 精确串 `(signal: 11, SIGSEGV` 或 `(signal: 6, SIGABRT` 且出自 `error: could not compile` 上下文行 | INFRA | 工具链崩溃，建议重试/报告 |
| 8 | disk_full | 日志含 `No space left on device` | INFRA | 走现状 infra_error 换机重试（DESIGN §3.5） |
| 9 | env_markers_raw | **零结构化 error 诊断**时，`looks_like_env_error` 的 raw-log marker 命中 | PROJECT_CONFIG | 现网 7b 语义；有结构化 error 时永不求值（被规则 2/3 先命中） |
| 10 | unknown | 无规则命中 | UNKNOWN | `失败原因未知（exit N）` + 尾部 ≤5 行摘录（过预算门） |

- **I1 的机械化**：`ATTR_CODE` 只能由规则 3/4 产出，二者都要求硬证据（结构化诊断 /
  已解析的 libtest 摘要）。原 v1 的"test 二进制崩溃 → CODE"规则因启发式不可测试而移除
  （评审 F8）：测试二进制 abort/SIGSEGV 落入规则 7 或 10。
- 规则 6/7/8/9 的日志匹配都只作用于 log_tail（200 行），不扫全量。
- 未来新增故障类 = 加一行规则，不改骨架。

### 1.6 呈现（agent 侧）

`format_result` 按 attribution 渲染，`agent_hint` 按 attribution 取词：

```
✗ 测试未能完成：编译 private_tun 时进程被 SIGKILL 杀死 [资源不足/疑似 OOM]
  证据 (log:1847): error: could not compile `private_tun` (lib) ... (signal: 9, SIGKILL: kill)
  非代码问题。已自动以 CARGO_PROFILE_TEST_DEBUG=0 重试（见下）
```

### 1.7 测试

fixture 驱动，一个故障类一个 fixture，至少覆盖（评审 F8.3）：
OOM（OOMKilled=true）、仅日志 SIGKILL 无 OOMKilled、worker 超时 137（必须 ≠ OOM）、
rustc SIGSEGV、test abort 无摘要（→ UNKNOWN）、磁盘满、纯编译错误、编译错误+日志 env marker
混合（→ CODE 不动摇）、测试失败（无 JSON 仅摘要行）、全 env 诊断。
现网既有单测（timeout 压倒、compile_error 不让位）必须全绿。

---

## 2. 机制二：任务语义契约（TaskContract）

### 2.1 现状病根

`task=test` 只是换了一条命令字符串（adapter.rs:163，裸 `cargo test --workspace`）：
`cache_config`（adapter.rs:181-201）对所有任务一视同仁，没有 `CARGO_PROFILE_TEST_DEBUG=0`
（dev profile debuginfo=2 是 OOM 的直接诱因，对"跑一下单测"零价值）；结论不含 pass/fail 数，
调用方要再发 `get_log(grep="test result")` 才知道跑了几个。

### 2.2 契约定义

每种 task 是一个五元组契约，在 rc-core 定义 trait，adapter 实现：

```rust
pub trait TaskContract {
    fn default_command(&self, flags: &TaskFlags) -> String;   // 现 command_for 职责迁入
    fn default_env(&self) -> &[(&'static str, &'static str)]; // 契约层默认环境
    fn parse_outcome(&self, exit_code: i32, log: &str) -> TaskOutcome;
    fn render_outcome(&self, o: &TaskOutcome) -> ConclusionParts;
    fn remediation(&self, rule: &str) -> Option<Remediation>; // 按规则名键控（评审 F3）
}
```

实现：`CheckContract`、`BuildContract`、`TestContract`、`CustomContract`。
**parser 启用条件（评审 F24）**：`TestContract::parse_outcome` 的 libtest 解析**仅在命令是
本契约生成的默认命令**时启用；profile 覆盖了 `tasks.test`（如 `cargo nextest run`）或
请求覆盖了 command 时，回退 `CustomOutcome(exit_code)`——不解析、不宣称。
未来为 nextest 等增加 parser = 新契约实现或显式 parser 选项。

### 2.3 TestContract 细则

- **默认环境**：`CARGO_PROFILE_TEST_DEBUG=0`、`CARGO_PROFILE_DEV_DEBUG=0`。
  **这是有意的行为变更**（评审 F12 建议 opt-in，裁决保留默认注入——反馈 #2 的明确产品
  决策）：失败测试的 backtrace 将无行号细节；需要 debuginfo 调试时用 profile env 或
  请求 env 覆盖（分层见 §2.4，用户覆盖永远赢）。文档与 CHANGELOG 显著标注。
- **最小摘要解析（PR1 就要，评审 F2）**：libtest 摘要行逐测试二进制累加：
  `test result: (ok|FAILED). N passed; M failed; K ignored; ...`
  失败用例名取自 `failures:` 清单块，记为 `(binary, test_name)` 对（评审 F24.3）。

```protobuf
message TestSummary {          // PR1：仅分类证据所需
  uint32 passed = 1;
  uint32 failed = 2;
  uint32 ignored = 3;
  uint32 binaries = 4;
  repeated string failed_names = 5; // "binary::test_name"，上限 20
  bool truncated = 6;
  bool summary_seen = 7;       // false = 未识别到任何摘要行（评审 F24：不得推断"未进入测试阶段"）
}
```

- **结论渲染**：`✓ 测试通过：47 passed, 2 ignored（3 个二进制）`；失败时列出失败用例名。
  `summary_seen=false` 时措辞为"未识别到测试摘要"，归因交给机制一（通常规则 3 编译错误
  或规则 10 未知），不做阶段推断。

### 2.4 显式 env 参数与 fingerprint 闭合（评审 F21，BLOCKER 修订）

请求级 env 转正为契约，核心原则：**执行语义与指纹语义必须出自同一份 effective profile**。

- `SubmitTaskReq` 增加 `map<string,string> env = 15`；MCP `check` 增加 `env` object 参数。
- **合并与指纹的唯一路径**：rc-core 暴露唯一的 `resolve_env() + canonicalize()`；
  分层合并（后者覆盖前者）：**adapter 全局默认 < 契约 default_env < profile env < 请求 env**，
  合并产物写回 `ResolvedProfile.env`，canonical 由合并后的 profile 生成，fingerprint 只算
  canonical。server 端权威重算（app.rs:202-211 现状）用同一函数；worker 只消费下发的
  effective profile，**不得**另行注入语义性 env。agent 本地 `ResultCache` 键用同一 canonical。
  禁止任何"hash 一份、执行 env 另一份"的实现。
- **契约 default_env 必须进 canonical**（评审 F11）：`CARGO_PROFILE_TEST_DEBUG=0` 若只放
  worker cache_config 而不进 canonical，旧结果（debuginfo=2）与新语义共享指纹 → 缓存投毒。
  同时 `EXECUTOR_ABI` abi2 → abi3（fingerprint.rs:68），并加兼容测试：升级后旧指纹不命中。
  ABI bump 触发器清单写进 fingerprint.rs 注释：契约 default_env 语义变更必 bump。
- **denylist 仅作用于请求 env**（评审 F11.2）：`PATH`、`HOME`、`RUSTC_WRAPPER`、
  `CARGO_HOME`、`RUSTUP_HOME`、`SCCACHE_*`、`CARGO_TARGET_DIR`。profile env 保持现状
  权限（现网允许 profile 覆盖 sccache 默认，不破坏）。校验：key `^[A-Za-z_][A-Za-z0-9_]*$`，
  单值 ≤4KB，≤32 个。
- `command` 覆盖与 `env` 同时出现：env 照常分层叠加（作用于容器环境），互不排斥；
  但 command 被覆盖时 TestContract parser 停用（§2.2）。

### 2.5 自动补救（按规则名键控，评审 F3/F8/F12）

```rust
pub struct Remediation {
    pub env_patch: Vec<(String, String)>,
    pub note: String,   // 呈现在结果里的一句话声明
}
```

- **自动重试白名单显式且封闭**：`{oom_killed, sigkill_suspected_oom}`。
  磁盘满/超时/未知一律不自动重试（磁盘满重试对 worker 盘满无效且烧预算，评审 F3）。
- 执行位置：agent 侧 Engine。命中白名单规则且本请求未曾重试 → 应用
  `remediation(rule)` 的 env_patch（经 §2.4 同一 resolve 路径 → 新 fingerprint）重提交一次。
- 结果声明：`⚠ 首次尝试疑似 OOM 失败，已自动以 CARGO_BUILD_JOBS=2 降并发重试并成功`。
  （2026-07-28 第二轮修订：TestContract 的 default_env 已含 `CARGO_PROFILE_*_DEBUG=0`，
  再以同样的 debug=0 重试是 no-op；补救 knob 改为降并发 `CARGO_BUILD_JOBS=2`。若
  effective env 与首次逐字节相同则跳过重试。）
  两次都失败：报**首次** verdict 为主结论（用首次 task_id 格式化），但附带第二次的
  task_id 与证据行（评审 F12.3，第二次日志可能更有信息，不丢弃）。
- 重试上限 1 次，无退避（同步零成本、任务本身有超时上限）；开关：agent 配置
  `auto_remediate = true`（默认开），单请求 `check(..., no_remediate=true)`。
  补救状态机挂在**任何**首次拿到 terminal verdict 的路径上（含 `get_result` 轮询）。

---

## 3. 机制三：上下文预算门 + 消息状态机

### 3.1 预算门（覆盖 #4）

rc-agent 建一个**所有 MCP 响应文本必经**的渲染出口 `BudgetGate`。

- **计量与截断**：一律按 UTF-8 字节计量、按字符边界截断（评审 F22.3）。
  单行上限 400 字节：中间省略，保头 250 + 尾 100，标注 `…[省略 N B]…`，同时给出
  原始行号与原始长度（`(log:1847, 原 4.1KB)`），调用方可 `get_log` 直达。
- **响应预算分槽（评审 F13.1）**，总上限 8KB，优先级从高到低、高优先级槽超额可挤占低槽：
  1. headline + verdict + 证据（永不截断——I1 优先于 I3）
  2. 结构化诊断 / TestSummary（≤4KB）
  3. Critical 通知（见 §3.2，每次必显）
  4. Info/Warning 通知（≤2KB）
  5. 其余装饰
- **get_log**：预算门在 agent 侧套用；响应携带 `next_offset` 与 `bytes_omitted`，行号与
  server 原始行号对齐（分页语义不因省略错位，评审 F13.3）。
  `raw=true`：跳过单行省略但**仍受响应总上限约束**；单行超出响应上限时，响应携带
  `line_byte_offset/next_byte_offset` 支持行内续读（评审 F22.1）。raw 不是无限灌入通道。
- **契约测试（I3 机械化）**：合成含 8KB 单行与超大 Unicode 单行的日志，断言任何工具响应
  ≤ 总上限、非 raw 模式无单行 > 400B、截断处为合法 UTF-8 边界、验证 headline/证据永在。

### 3.2 消息状态机（覆盖 #7）

通知统一走 Notice API，**快照语义**（评审 F23.2）：每次调用，生产者产出本次的完整
notice 快照；状态机对比上一快照决定呈现。

```rust
pub enum NoticeSeverity { Info, Warning, Critical }
pub struct Notice {
    pub category: &'static str,       // "sync_roots" | "exclude" | "egress_pending" | ...
    pub severity: NoticeSeverity,
    pub text: String,                 // 全文
    pub compact: String,              // 一行自包含摘要（Critical 重复时用）
    pub identity: [u8; 32],           // blake3(category ‖ 结构化字段规范序，如 sorted hosts)（评审 F18）
}
```

状态键：`(project_id, worktree_id, category)`（评审 F23.1——常驻 agent 服务多项目/多
worktree 不得串）。转移表：

| 事件 | Info/Warning | Critical（exclude、baseline-off、egress-refused 等影响结果解释的） |
|------|--------------|--------------------------------------------------------------------|
| 首次出现 | 全文 | 全文 |
| identity 不变的重复 | 沉默 | **每次必显 compact 一行**（评审 F13.2/F23：撤回 v1 的"压缩或沉默"——多 LLM 会话共享 agent 进程时"同前"不可依赖；正确性关键信息不赌调用方记性） |
| identity 变化 | 全文 | 全文 |
| 消失后复发 | 视为首次（快照对比天然处理，评审 F23.2） | 同左 |

- agent 进程重启即遗忘：安全偏置（最多多说一次，不会漏），可接受，不做持久化。
- 现有生产者（describe_roots/exclusions/inclusions/egress 三段、scanner baseline 警告）
  全部迁移为 Notice；engine.rs:879-881 的"每次全文重复"决定由"Critical 每次 compact 一行"
  替代——信息保真（每次都在），噪音消除（一行 vs 整段）。
- 未提交的 egress 通知恰是首批受益者。

---

## 4. 机制四：基线与增量语义（诊断 delta）

### 4.1 存储基础

每任务诊断（≤50 条）在 `tasks.result_json`（store.rs:690-719），tasks 表已有
`project_id`、`worktree_id`、`task_type`、`fingerprint`（含索引）。缺的只是比较逻辑。
**基线选择不依赖机制一的 verdict**（旧 `result_kind` 足够，评审 F16.3 修正依赖图）。

### 4.2 诊断身份键（评审 F6/F25 修订）

```
strict_key = blake3(level ‖ code ‖ file_path ‖ normalize_spans(message))
```

- `normalize_spans`：**仅**剥离消息中可精确识别的 span 片段（正则
  `\S+\.rs:\d+:\d+` 及裸 `:\d+:\d+` 尾缀）并折叠空白。**不做通用数字剥除**——
  `[u8; 32]` vs `[u8; 64]`、"expected 2 arguments" 里的数字是语义，剥掉会假合并（F25.1）。
- 顶层行号不参与键（编辑漂移免疫）；同键多条按出现次数配对。
- 两级匹配（F25.2）：strict_key 算确定 delta；fuzzy（去 file_path，识别 rename）只产
  "疑似移动"提示，**不计入** new/fixed。
- **approximate 条件**（F6.2/F25.3）：任一侧 `truncated_diagnostics>0`、任一诊断 `code`
  为空（parse_generic 路径 code 恒空）、或检测到疑似 rename → `approximate=true`，
  且 **truncated 时不报告 fixed_count**（假"已修复"比不报更糟，F6.3）。

### 4.3 基线选择（评审 F4/F5 修订）

- 请求语义：`baseline = auto（默认）| none | last_success | <task_id>`。
- **`auto` = 同 `(project_id, worktree_id, task_type)` 最近一次完成任务**——
  "相对上次迭代的增量"正是反馈 #5 的场景；不跨 worktree（B worktree 首次 check 若无
  本 worktree 历史 → 视同 none，绝不借用 A 的基线，F5 的串分支场景堵死）。
- `last_success` 显式模式 = 同键最近一次 result_kind=success 的任务（"相对上次绿"）。
- 跨 worktree 比较必须显式 `<task_id>`。
- server 在 get_task 响应读时计算（两份 result_json 集合差，无新表）：

```protobuf
message DiagDelta {
  uint32 new_count = 1;
  uint32 fixed_count = 2;          // truncated 时不填（见 §4.2）
  uint32 preexisting_count = 3;
  repeated Diagnostic new_diagnostics = 4;
  repeated string preexisting_summary = 5;  // 按 crate 聚合一行
  string baseline_task_id = 6;
  bool approximate = 7;
}
```

### 4.4 呈现

```
✗ 2 errors（新增 1，基线已有 1）[代码问题]
  新增:
  crates/foo/src/lib.rs:42:9 E0308 mismatched types …
  已修复 3 条。基线已有 1 条（rrd-load-balancer）——详情 get_log。
```

- 新增全文（占用 max_diagnostics=10 额度优先）；已修复报数；基线已有压一行按 crate 聚合。
- 文案说"基线已有"，**不说"非本次改动引入"**——approximate 场景下因果断言会撒谎（F25.3）。
- warning 同样参与 delta。"存量集合固化为项目级 baseline 文件"留作后续，本期不做。

---

## 5. 机制五：流式生命周期

### 5.1 现状三缺口（评审 F10 修正事实）

1. **有流无解析**：docker 日志已经以 `follow: true` 流式进入内存缓冲
   （docker.rs:323-354），但没有任何中间解析与 `TaskProgress` 上报——进度不可见的原因
   是缺 progress 事件，不是缺流式 API。432s 的构建轮询 7 次全是 `当前阶段: building`。
2. **等不明白**：每任务 `build_ms` 已落库（schema.sql:134-137），但无按项目聚合查询。
3. **停不下来**：取消链路存在（admin cancel → `CancelTaskId` → `Runner::cancel` →
   `sandbox.kill`），唯独没暴露给 agent。注意 supersede（app.rs:379-420）只清
   **未执行**任务（`is_cancelable` 仅 pending/syncing/queued），与杀 running 是两条路。

### 5.2 单元进度（评审 F26 修订）

- 在现有流式回调处逐行匹配 `^\s+(Compiling|Checking) (\S+) v`。**这是单元开始事件**：
  字段名 `units_seen`（不叫 done）、`current_unit`（正在处理的 crate 名）。
  `Fresh` 行非 verbose 下不稳定出现，不依赖。
- **collector 与 emitter 解耦（F26.3）**：日志收集永不等待控制面；进度走有界
  latest-value channel（tokio watch），emitter 节流 ≥2s 取最新值上报。
- **server 不把每条进度写 task_events**（行膨胀 + 单控制面写锁，F14.4/F26.3）：
  per-task 内存内 `progress_snapshot`（crate 名、units_seen、单调 `progress_version`），
  task_events 仍只记 phase 转移。

### 5.3 历史参考（不是 ETA，评审 F26.2）

- `TaskResult` 新增 `uint32 units_seen_total`，完成时随 stats 落库（tasks 表加列，additive）。
- store 新增 `history_ref(project_id, task_type, n=20)` → 最近 N 次完成任务的
  build_ms p50 与上次 units_seen_total。
- 渲染定调为**参考**，不承诺剩余时间、不渲染确定分数：
  `⏳ building: rrd-core（本次已见 23 个单元；上次同类任务共 107 个、耗时 p50 180s，参考）已运行 96s`
  冷热缓存/worker 差异使历史不可比的风险由"参考"措辞承担；分桶精化留作后续。

### 5.4 长轮询语义扩展（评审 F14 修订）

get_task 请求新增两个字段：`bool return_on_progress = N`（presence 明确，默认 false =
旧行为原样保留）与 `uint64 seen_progress_version = N+1`。
`return_on_progress=true` 时：`progress_version > seen` 或终态即返回；响应携带
`progress_version`，客户端下次原样回传——游标闭环（F14 指出的 v1 自相矛盾就此消除）。
旧 agent 不发新字段 → 行为与今天完全一致。

### 5.5 取消（评审 F7 修订）

- 新 agent RPC `CancelTask { task_id }` + MCP 工具 `cancel(task_id)`。
- **镜像 admin cancel 路径**（admin.rs:613-637）：server 先置 terminal 状态
  （canceled）并**由 server 写入 verdict = CANCELED**，再向 worker 发 `CancelTaskId`；
  late result 由现有 `on_task_done` 的 terminal 丢弃逻辑处理（app.rs:633-636）。
  classify 永不产出 CANCELED（v1 规则 2 删除）；不复用 supersede。
- 归属校验：task 行的 `project_id` 必须与调用方 agent 的项目身份一致。
- 被取消任务不进结果缓存（terminal 非 success/compile_error，天然满足）。

---

## 6. 兼容性与迁移

- **proto 全部 additive**；server 容忍无 verdict 的旧结果（渲染回退旧 kind 路径）。
- **EXECUTOR_ABI**：abi2 → abi3（契约 default_env 改变执行语义，§2.4）。bump 触发器
  清单写入 fingerprint.rs 注释。附兼容测试：旧指纹不命中新结果。
- **schema additive**：tasks 加 `units_seen_total` 列；无新表。
- **依赖图（评审 F16.3 修正）**：规则 4 依赖最小 TestSummary（同在 PR1）；自动补救依赖
  正确 RESOURCE 分类（PR1）；delta 只依赖旧 result_kind（可独立于 verdict 上线）；
  预算门与 delta 呈现有交互（新增诊断占预算优先）。
- **每机制独立 kill-switch**（评审 F16.2）：`verdict_v2`、`task_contract_env`、
  `budget_gate`、`diag_delta`、`unit_progress` 五个 agent/server 配置开关，可单独回滚。
- **前置**：工作区未提交的 egress 工作先落地再动工；其新增通知随 §3.2 一并迁 Notice。

## 7. 实施切分（PR 序）

| PR | 内容 | 验收（对应原反馈） |
|----|------|--------------------|
| 1 | proto 新增字段 + worker 证据采集（inspect 于 remove 前）+ 规则表分类器（含**最小 libtest 摘要解析**）+ §1.3 映射表 + 本地缓存过滤 + agent 渲染 | OOM/疑似 OOM 报"资源不足"带证据行号；137 双关分开；test 失败仍 CODE **无过渡回退**（#1）；§1.7 fixture 全绿 |
| 2 | TaskContract + TestSummary 完整化 + env 参数与 effective profile 闭环 + ABI bump + 自动补救 | test 结论含 pass/fail 与失败用例名（#3）；OOM 自动降配重试一次并声明（#2）；同 manifest 仅 env 不同 → fingerprint 不同且 worker 所见 env 正确（评审 F21.3 端到端测试） |
| 3 | BudgetGate + Notice 状态机迁移 | 合成 8KB 行/Unicode 行过契约测试；样板首次全文后续静默、Critical 每次一行（#4 #7） |
| 4 | 诊断 delta | 同 worktree 增量三分类；跨 worktree 不串基线；truncated 不报 fixed（#5） |
| 5 | 单元进度 + 历史参考 + return_on_progress + CancelTask | 轮询可见当前 crate 与 units_seen；cancel 生效（#6） |

## 8. 后续（本期不做，评审 F16.4 降级）

- "结论后追查率"埋点（verdict.rule × 完成后短窗内 get_log 率）——自我度量闭环，
  待五机制落地后作为独立控制面工作。
- 存量诊断固化为项目级 baseline 文件；ETA 按冷热/worker 分桶。

## 9. 评审裁决记录

评审全文 `.omc/research/llm-contract-design-review.md`（26 条：5 BLOCKER / 16 MAJOR /
4 MINOR / 1 NIT）。裁决：

- **接受并已并入**：F1(§1.3 映射表、磁盘满归 INFRA)、F2(最小摘要解析进 PR1)、
  F3(补救按规则名+白名单)、F4/F5(基线键改 project_id+worktree_id+task_type)、
  F6/F25(身份键收紧、approximate、truncated 不报 fixed)、F7(cancel 镜像 admin、server 写
  CANCELED)、F8(移除 test 崩溃启发式；SIGKILL 收紧为 cargo 精确串)、F9(compile_error
  不让位不变量保留)、F10(§5.1 事实修正)、F11/F21(env 进 canonical、唯一 canonicalizer、
  denylist 限请求 env)、F13/F23(预算分槽、Critical 每次必显、Notice 键含项目/worktree、
  快照语义)、F14(return_on_progress + progress_version 游标闭环、进度不写 task_events)、
  F17(env_hints 续填、删 ATTR_TOOL)、F18(identity 用结构化字段)、F19(timeout 只看
  timed_out)、F20(措辞收窄、command+env 合并规则)、F22(raw 仍受限+行内续读、UTF-8 字节
  计量)、F24(summary_seen、parser 仅默认命令、失败名带 binary)、F26(units_seen、
  历史参考非 ETA、collector 解耦)、F15(进度幂等；日志内存尖峰风险**明示接受**，spool 留后续)。
- **有修改地接受**：F8 之"疑似 SIGKILL 禁自动补救"——改为允许（cargo 精确串匹配 +
  共享宿主机上 OOMKilled 可为 false 的现实 + 重试成本有界）。
- **拒绝**：F12 之"debug=0 改 opt-in"（反馈 #2 的明确产品决策，保留默认，标注行为变更）；
  F16 之"仅交付最小切片"（范围为明确指示：五机制全做；其解耦/开关/依赖修正已采纳）。
