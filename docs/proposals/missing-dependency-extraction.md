# 从构建日志提取缺失依赖 v1

状态：**已实现并上线验证**。经 gpt-5.6-sol(high) 五轮对抗评审，findings
10 → 8 → 7 → 6 → 2 收敛，共 33 条，其中相当一部分是前一轮修复自身引入的。
评审记录见 §9。
关联：§3.5、§8.3、§8.5、§10.3、§11。

> **先读 §8**：本提案顺带查出并修复了 3 个既有问题——两个是现网真实存在的
> （镜像健康度、缓存命中），一个是潜伏的（迁移机制在有第一个迁移步骤之前不会发作）。
> 其中"一个项目缺库会把 fleet 唯一镜像踢出轮换"的影响面大于本提案本身。

## 1. 问题

分类正确只解决了一半。`rrd-sys` 缺 `librrd` 时，agent 拿到的全部信息是：

```
✗ 环境错误（exit 101）：error: failed to run custom build command for `rrd-sys v0.1.3`
环境缺依赖：用 list_envs 找可用镜像，或 prepare_env 提交 Dockerfile。
```

**没说缺的是什么。** 而证据就在日志里：

```
cargo:rerun-if-env-changed=LIBRRD_NO_PKG_CONFIG
Could not find librrd
```

三份真实日志里这行落在 1342 行的第 45 行、1340 行的第 47 行、3259 行的第 1970 行——
位置没有规律，agent 无从知道该看哪儿。要 `grep` 就得先知道 `librrd` 这个词，
而那正是它想知道的东西；不 grep 就只能整段翻。两条路都在花掉这个系统存在的意义
（§11 省 context）。

这不是个例。zfc 的 `Cargo.lock` 有 1401 个包，其中 **27 个 `-sys` crate**——
`aws-lc-sys`、`openssl-sys`、`libsqlite3-sys`、`zstd-sys`、`libdbus-sys`……
参考镜像预装了 pkg-config、libssl-dev、protobuf-compiler、cmake、libclang-dev，
本身就说明当初知道这类问题存在，只是用"把常见的塞进镜像"来挡，不 scale。

已有的 `adapter::system_dep_hints` 不解决这个：它只在**压根没有可用镜像**时被调用
（`engine.rs`），构建失败时根本不走；而且是 11 条硬编码 needle 扫 `Cargo.toml`，
没听说过 rrd-sys。

## 2. 目标与非目标

**目标**：构建失败时，把日志里点名的缺失依赖提到结果里，一次调用可见。

**非目标**：
- 不自动装包。沙箱只读根、非 root、出网白名单，运行时 apt 是安全倒退不是功能。
- 不自动生成并提交 Dockerfile。`prepare_env` 仍需人工审批（§8.3）。
- 不追求覆盖所有失败形态。宁可少报，不可错报——见 §3。

## 3. 两条铁律

错答案比不答案贵：按错的包名行动意味着**让人类审批一个修不好问题的镜像**（§8.3）。

### 3.1 库名是事实，包名是推测，二者必须可区分

`<x> → lib<x>-dev` 这个惯例对 `librrd`、`zstd` 成立，对 `openssl`（`libssl-dev`）、
`alsa`（`libasound2-dev`）不成立。所以已知映射表优先，落到惯例的一律标注推测。

映射表按**库/可执行文件分开**。同一个词在两边是不同的包：

| 名字 | 作为命令 | 作为库 |
|---|---|---|
| `curl` | `curl` | `libcurl4-openssl-dev` |
| `python` | `python-is-python3` | — |
| `Python.h` | — | `python3-dev` |

一张表必然把其中一个搞错**还声称确定**。

不认识的可执行文件**不给猜测**：二进制名与提供它的包之间没有任何命名规律，
编一个 `libprotoc-dev` 比不说更糟。失败的 `-sys` crate 同样不给——它因 vendored
源码、配置、自身 panic 而失败的概率不低于缺库（`aws-lc-sys` 就是典型）。

### 3.2 猜测需要形状，不能只凭"没找到"

`Could not find X` 是普通英语，cargo 自己的输出里到处都是。所以只在 X **是已知名字
或形如 `lib…`** 时才采信。

这里走过弯路：最初用英文单词黑名单挡 `directory`/`file`，评审一句话点破——
黑名单永远列不全（`repository` 就漏了）。改成白名单：用形状证明，而不是用穷举排除。
白名单还要再挡一层文件后缀，因为 `lib.rs` 也满足 `starts_with("lib")`。

## 4. 识别的形态

| 信号 | 来源 |
|---|---|
| `Package X was not found in the pkg-config search path` / `No package 'X' found` | pkg-config |
| ``system library `X` required by crate`` | pkg-config-rs |
| `cannot find -lX` / `could not find native static library X` | ld / rustc |
| `X.h: No such file or directory` / `'X.h' file not found` | gcc / clang |
| `X: command not found` / `zsh:1: command not found: X` / `sh: 1: X: not found` | bash / zsh / dash·ksh |
| ``Is `X` installed`` / `failed to find tool "X"` | cc crate |
| ``linker `X` not found`` | rustc |
| `Could NOT find X` | CMake `find_package` |
| ``Could not find `X` `` / `Could not find X`（行尾） | 构建脚本自己的探测 |
| `Unable to find X` | bindgen |

**证据强度分级**（`Confidence`）：工具明说 > 可辨认的散文 > 只知道哪个 crate 失败。
同名时强证据取代弱证据——`Could not find ninja` 之后的 ``Is `ninja` installed?``
必须落到程序而不是猜出来的 `libninja-dev`。

**同一事实只算一次**：pkg-config 模块、`-l` 名、头文件属于同一"类"（缺库），
ld 和 pkg-config 报同一个库不该占两个名额。而 `curl` 命令与 `curl` 库是两件事。
平手时按"能解析出真包名"决胜，否则日志顺序会决定答案。

**上限 12 条**，收集缓冲 48 条后按强度排序截断——否则坏掉的 sysroot 刷出几千行
`cannot find -l…`，又把 context 塞满了。缓冲满时强证据驱逐最弱的，不能让先到的
弱证据把后到的真答案挡在外面。

## 5. 必须绕开 cargo 的 JSON

真实日志 1342 行里 **1294 行是 `--message-format=json`**，其中密布包名和 feature
列表。实测有 `"features":[…,"pkg-config","vcpkg"]`，与"失败的 pkg-config 调用"只差
一个空格。人类可读的那一半必然也带着同样的信号，所以**整段跳过以 `{` 开头的行**——
和 `parse_cargo_json` 同一条规则，反着用。

## 6. 缺头文件要改判（§3.5 risk #4）

`foo.c:3:10: fatal error: openssl/ssl.h: No such file` 会被通用适配器（§10.3）
解析成一条**格式完好的 error 诊断** → `compile_error` → agent 被支去改没错的代码。

改判为 `env_error`，但**只在所有 error 诊断都是这种形状时**。混有真编译错误就仍算
代码问题——反向误判会把 agent 真正该修的诊断藏起来，更危险。

这一条在评审里来回摆了三次，是整个提案最难的地方：

1. 扫诊断的 `rendered` → `let _: i32 = "cannot find -lssl";` 的类型错误被读成缺库，
   因为 rendered 里带着**用户源码**。
2. 改成只扫 `message` → 又漏掉链接失败，因为 `error: linking with \`cc\` failed`
   的真正原因只在 note 里。
3. 最终：扫 message + rustc **自己的** `= note:`/`= help:` 续行。源码引用块由
   `-->` 引入，遇到它才终止——不能用 `|` 判断，链接器包装器会在 note 里打印表格。

另有一条靠措辞无法区分：`compile_error!("openssl/ssl.h: No such file")` 的 message
与真的缺头文件**逐字相同**。改用来源判定：`.rs` 文件里不可能缺 C 头文件。

代价是 proc macro 透出的真实缺头文件会回落到 `compile_error`。**这是有意的取舍**：
`compile_error!` 带任意文本在校验 feature flag 的 crate 里遍地都是，而 proc macro
透出 C 头文件很罕见；且回落正是本功能存在之前的行为，那个场景没有失去它本来有的东西。
评审第五轮认可了这个取舍。

## 7. pkg-config 自己缺失时

工具不在时**每个探测都会失败**，那些探测携带的模块名不是证据。此时只报
`pkg-config`，并丢弃所有 PkgConfig 类发现。

但"缺失"的判据必须严格。这两条**不是**缺失：

- `pkg-config has not been configured to support cross-compilation` —— 这是
  pkg-config 0.3 的 `CrossCompilation` 错误，二进制就在那儿。
- `Could NOT find PkgConfig: Found unsuitable version "0.29.2"` —— CMake 的版本
  抱怨，还顺便告诉你二进制在哪。

误判成缺失不但建议一个装了没用的包，还会**把真正的证据删掉**。

## 8. 评审顺带查出的既有问题 —— **已全部修复**

### 8.1 一个项目缺库会把 fleet 唯一镜像踢出轮换（影响面最大）

zfc 缺 librrd 连续三次 `env_error`，`record_image_outcome` 把镜像标成 `failing`，
**所有项目**都用不了。而且 `failing` 是单向门：SQL 只有 `healthy → failing`，
成功只把计数清零，永不恢复。

本提案让 `env_error` 变得可行动，反而更容易被重试触发，所以必须一起修：

- 日志点名了要装什么 → 那是项目需求的陈述，与镜像无关，**不计**。
- 同一项目再次失败 → 一个事实，不是三个，**不计**。
- 只有**不同项目**连续失败才指向镜像。
- 成功把 `failing` 恢复 `healthy`。

### 8.2 迁移机制分不出新库和 v0 旧库

加列时暴露：`user_version` 对全新库和未迁移的旧库**都读 0**，新步骤会被套用到
`schema.sql` 刚建好、已带该列的库上（`duplicate column name`）。改为查表判断新旧。
线上库从 `user_version=1` 干净升到 2。

### 8.3 缓存命中比未命中更没用

本地指纹缓存只存 `result.kind`，第二次 check 把 `env_error` 重放成光秃秃一个词——
恰好在 agent 因为它而重试时把提示丢掉。改为存序列化的 `TaskResult`。

注意**不能存渲染好的文本**：那会把当次的 `max_diagnostics` 和 `synced=1.0MB` 冻进
去，被一次根本没走网络的命中重放出来。按当前参数重新渲染，字节数归零。

## 9. 评审记录

五轮，33 条。前四轮的高危项**多数是前一轮修复自身引入的**：

**第一轮（10 条）**
- `#[serde(default)]` 开得太大：会让被截断的 `ResolvedProfile` 反序列化成默认值，
  绕过 `app.rs` 里"存储的 profile 读不出来就判任务失败"的设计，把错误配置发给
  worker。收窄到单个字段，并加测试断言截断的 profile 仍然失败。
- 库/程序共用一张映射表（见 §3.1）。
- 从 `-sys` crate 名推包名（见 §3.1）。
- `_NO_PKG_CONFIG` 关联逻辑一口气吃了三条 High，且在真实日志上根本没起作用——
  **直接删掉，没有打补丁**。
- 英文单词黑名单方向错了（见 §3.2）。

**第二轮（8 条）**
- 扫 `rendered` 把用户源码读成缺库（见 §6）。
- 把 CMake 的 cross-compilation 错误当成 pkg-config 缺失（见 §7）。
- 三个标"确定"却错的包名：`python`→应为 `python-is-python3`、`libtool`→`libtool-bin`、
  `libmysqlclient-dev` Bookworm 根本不发→`default-libmysqlclient-dev`。
  标"确定"却是错的，比标"推测"更有害。

**第三轮（7 条）**
- 只扫 `message` 又漏掉链接失败（见 §6）。
- 我上一轮加的 `pkgconfig → pkg-config` 映射本身是错的：Debian 上没有 `pkgconfig`
  这个命令。该别名只在 CMake 语境下成立，挪到匹配点处理。
- 同名跨类算两条，把 12 条额度花两遍。

**第四轮（6 条，1 条主动拒绝）**
- note 续行不重复 `= note:` 前缀，只留首行会丢掉关键那句。
- 诊断证据被当成"raw 为空才用"的兜底，改为两个来源合并分析。
- 拒绝 proc-macro 那条，理由见 §6。

**第五轮（2 条）**——明确确认 `analyze_parts`、抑制条件、`quality()`、34MB 日志
复杂度均无问题，并认可 §6 的取舍。
- zsh 写作 `zsh:1: command not found: foo`，行号被当成了程序名；ksh 不在识别范围。
- note 里的 `|` 会把 note 提前截断，而链接器包装器恰好在表格后才打印缺库那行。

## 10. 实现落点

| 文件 | 内容 |
|---|---|
| `rc-core/src/envdep.rs` | 全部提取逻辑与包名映射（新增） |
| `rc-core/src/diag.rs` | `Classification.env_hints`、`is_environment_diagnostic`、`diagnostic_evidence` |
| `rc-core/proto/rc.proto` | `TaskResult.env_hints = 10` |
| `rc-core/build.rs` | 仅 `.rc.v1.TaskResult.env_hints` 加 `#[serde(default)]` |
| `rc-worker/src/runner.rs` | 写入结果 |
| `rc-agent/src/engine.rs` | `format_result` 渲染；缓存改存 `TaskResult` |
| `rc-agent/src/index.rs` | `ResultCache` 加 `kind`/`result` 列 |
| `rc-server/src/store.rs` | `record_image_outcome` 重写；迁移步骤 1 |
| `web/src/pages/TaskDetail.tsx` | 无诊断时展示 env_hints |

## 11. 上线验证

真实 zfc，就是当初那个失败：

```
✗ 环境错误（exit 101）：error: failed to run custom build command for `rrd-sys v0.1.3` [env_error]
构建日志显示环境缺少以下依赖:
  - pkg-config 模块 `librrd` 未找到 → 可能是 librrd-dev（按命名惯例推测，需核实）
  - crate `rrd-sys v0.1.3` 的构建脚本失败（原因未必是缺依赖）
  安装建议: apt-get install -y librrd-dev
```

- 三份真实生产日志（1340/1342/3259 行，依赖树里另有 26 个 `-sys` crate 可供误报）：
  只提取出 `librrd` + 失败的 crate，**零误报**；两份无关的 env_error 日志
  （缺 `/private_tun`、rustup 只读）：零输出。
- 34MB / 53680 行日志 66ms。
- 连跑四次 zfc，镜像保持 `healthy consec=0`（修复前三次即废）；同一镜像上
  ci_compile_rs 仍 `✓ success`。
- 413 单元测试，clippy 干净。

## 12. 已知盲区

- **包名只覆盖 Debian Bookworm**，与参考镜像一致。换发行版基底则推测部分失效
  （已知映射仍对，惯例部分不对）。
- **链接失败在 cargo JSON 下仍可能报成 `compile_error`**：若 rustc 把原因放在
  message 和 note 之外的位置，两处都读不到。
- **`system_dep_hints` 未统一**：它仍只在"没有任何可用镜像"时被调用，扫的是源码而
  非日志。两者输入不同，暂不合并。
