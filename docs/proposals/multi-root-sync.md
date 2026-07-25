# 提案：多根同步（multi-root sync）v2

状态：**提案，未实现**。v1 经 gpt-5.6-sol(high) 对抗评审判定 REDESIGN，本文是重写版。
评审记录见 §15。
关联：§3.1、§4.1、§4.2、§4.3、§5.1、§7.1、§7.3、§16。

> **先读 §14**：评审过程中发现了 6 个与本提案无关的**现网既有 bug**，其中一个
> （symlink 全部损坏）严重程度高于本提案本身。**它们已经全部修复并合入**，
> 多根同步本身仍未实现。

## 1. 问题

cargo 的 path 依赖可以指向仓库之外。实测 `../zfc`：

```
zf-worker/Cargo.toml → shadow-tls-tokio { path = "../../shadow-tls-tokio" }
common/Cargo.toml    → private_tun      { path = "../../private_tun" }
```

rc-agent 只扫调用方给的那一个根，worker 把它挂在 `/work`，`../private_tun` 在容器里
解析成 `/private_tun`：

```
error: failed to load manifest for workspace member `/work/zf-vless-enc`
Caused by: failed to read `/private_tun/Cargo.toml` (os error 2)
```

`.remote-compile.toml` 里没有任何字段能表达"再带上这几个兄弟目录"。

## 2. 目标与非目标

**目标**

1. 一个任务可同步 N 个目录，并在容器里保持**原样的相对位置**，使 `../private_tun`
   无需改写即可解析；
2. 单根项目**逐字节不变**：同样的 `root_hash`、同样的 `/work` 布局、同样的 worktree 复用；
3. 发现不全时**失败关闭**，绝不退化成一个"看起来成功"的旧缓存；
4. 仓库外目录离开开发机之前，用户有机会拒绝。

**非目标**

- 不改写用户的 `Cargo.toml`；
- 额外根的 L1 基线优化（见 §5，v1 曾试图做，是它崩掉的主因）；
- 非 Rust 适配器的发现实现。

## 3. 核心机制：anchor 坐标系

唯一不变量：**主根到每个额外根的相对路径，在容器里必须与本地一致**。

取所有根的最深公共祖先为 **anchor**，映射到 `/work`：

```
本地                                            容器
/Users/lulin/code/github/          (anchor)  →  /work
/Users/lulin/code/github/zfc       (primary) →  /work/zfc          ← workdir
/Users/lulin/code/github/private_tun         →  /work/private_tun
/Users/lulin/code/github/shadow-tls-tokio    →  /work/shadow-tls-tokio
```

anchor 只是坐标系，**不被扫描**；中间目录在 worker 上建成空目录。

**单根退化**：只有一个根时 anchor 就是它自己，挂载点 `/work`，与现状一致——
这是"现存项目零影响"的来源。

**深度上限**：相对深度 > 8 直接拒绝，不静默生成 `/work/Users/lulin/...`。

**根必须提升到 git top-level**。cargo 报告的 path 指向 *package* 目录，可能是某个仓库的
子目录（`/shared/crates/foo`，git 根是 `/shared`）。若直接把 package 目录当根：
`git ls-files` 给出的是 package 相对路径，`git rev-parse HEAD` 给出的是**整个仓库**的 HEAD，
worker 会把整个仓库 archive 进 `foo` 的挂载点——文件全部落错位置。
所以每个发现到的路径先 `git_root()` 提升，anchor 在提升后的根之上计算。
主根本来就这么做（`engine.rs:resolve_root`），保持一致。

**拒绝符号链接根**。scanner 会 `canonicalize` 根路径，而 cargo 按字面量解析：
`/ws/lib -> /opt/lib` 时 cargo 要的是 `/work/ws/lib`，canonical 化会挂到 `/work/opt/lib`，
不变量被打破。构造 alias 链接的机器不值当，直接拒绝并说明原因。

## 4. 一个扁平 manifest（v1 的多 manifest 方案已废弃）

v1 给每个根一份 `entries` + 自己的 `base_commit` / `baseline` / `project_id` / mirror。
评审指出这条路径同时引入了 L1 错位（§3 已述）、跨 worktree 的 mirror 竞争、
双形态 manifest、以及大部分服务端改动。

v2：**所有根的条目合并进现有的单个 `entries`，路径改为 anchor 相对**。

```proto
message Manifest {
  repeated FileEntry entries = 1;   // anchor 相对路径
  string root_hash            = 2;
  string base_commit          = 3;  // 主根的
  bool   baseline             = 4;
  string anchor_mount         = 5;  // 主根相对 anchor；单根为 ""
  repeated RootInfo roots     = 6;  // 仅用于披露与可观测
}

message RootInfo {
  string mount     = 1;
  string local_path= 2;
  bool   primary   = 3;
  uint64 bytes     = 4;
  uint32 files     = 5;
}
```

- **额外根一律走 L2**（`in_baseline = false`），不做 mirror / bundle。
  这不是妥协，而是沿用 §4.3 对 submodule 的既定结论："子模块文件一律走 L2 内容寻址同步，
  不为其做 L1 mirror/bundle……CAS 全局去重使重复同步零成本，仅首次全量上传一次。"
  外部 path 依赖和 vendored submodule 是同一类东西，同一个理由适用。
- 主根的 L1 不变：baseline extract 到 `/work/<anchor_mount>`，manifest 路径已含该前缀。
- `root_hash` **无需新算法**：单根时 `anchor_mount == ""`，路径与今天逐字节相同，
  `root_hash(entries)` 的结果自然不变（`manifest.rs:28` 只吃 path/size/hash/type/exec）。
  多根时路径带前缀，hash 自然改变。v1 的 H2 构造是多余的。
- worker 侧 `workspace::plan / apply_deletions / verify` **对整个 anchor 跑一次**，
  §7.3"manifest 即真相"天然覆盖全树，不需要逐根 + 顶层清扫两段逻辑。

代价：`private_tun` 首次全量 L2 上传（约几十 MB），之后 CAS 去重。可接受，且与 submodule 现状一致。

## 5. 指纹必须带布局版本

**这是评审找到的最隐蔽的一个洞。** §8 要修的挂载 bug 会改变构建语义
（`path` 子项目从"编整个 workspace"变成"编子项目"），但 manifest 和 profile 都没变，
于是 `fingerprint` 不变 → rc-agent 的本地 `ResultCache`（`engine.rs:135`）和服务端任务缓存
（`app.rs:194`）会直接返回**语义已失效的旧结果**，worker 根本不会被调用。

`FingerprintInput`（`fingerprint.rs:13`）目前只有 manifest_root_hash / image_digest /
toolchain / profile_canonical，没有任何执行器版本维度。

修法：加 `executor_abi: &'static str`，值随布局/执行语义的每次变更递增（`"l2"`）。
同时给本地 `results.sqlite` 加版本前缀，服务端旧结果按 abi 失效。
目标卷也换命名空间，避免旧 `path` 语义留下的 `target/` 被复用。

## 6. 发现机制

adapter trait：

```rust
fn extra_roots(&self, root: &Path) -> Result<Discovery, RootDiscoveryError>;
```

`Discovery` 带 `complete: bool`。generic adapter 返回空且 complete。

### 6.1 主路径：`cargo metadata --no-deps --offline --format-version 1`

已在 zfc 上实测：`packages[].dependencies[].path` 是 cargo 解析好的绝对路径，
覆盖三类依赖（normal / dev / build）与 `[workspace.dependencies]` 继承，离线亚秒级。

**但只看 dependencies 不够**——评审指出的补充来源：

1. **`packages[].manifest_path` 落在主根之外**：`members = ["../plugins/*"]` 这类外部成员
   会出现在 `packages[]` 里，但未必是任何人的 `dependencies`。直接扫 `manifest_path` 更全，
   且把 dependencies 那条路也覆盖了；
2. **`[patch.*]`**：`cargo metadata` 不输出 patch。必须读 `metadata.workspace_root` 的清单
   ——cargo **只认 workspace 根的 patch/replace**，成员级声明被忽略，v1 说的"每个根的清单"不准确。
   zfc 的 Cargo.toml 里就写着"迭代 Brutal 时可临时改 `path = "../smoltcp"`"，是真实用法；
3. **`.cargo/config.toml` 的 `[patch.*]`**：config 级 patch 同样有效；
4. **`[replace]`**：已废弃但仍生效。

**递归**：额外根自身的 path 依赖不在主 workspace 的 metadata 里
（zfc 的 `private_tun/tcp_over_multi_tcp_client` 就是），对每个新根重跑，
带 visited 集合与深度上限（4）防环。

### 6.2 失败关闭

`cargo metadata` 会因语法错、缺失的 path 目标（尚未由 `pre_commands` 生成）、
冷 lockfile 下的 `--offline`（文档措辞是 "if possible"）而失败。

v1 说"降级到 TOML 解析 + warning"。评审指出这不够：漏掉一个额外根会重新生成
**与单根时代完全相同的 fingerprint**，而 `engine.rs:135` 的本地缓存命中路径
在 `with_warnings` 之前就 return 了——用户拿到一个没有任何警告的旧成功结果。

所以：发现不完整时 `complete = false`，**拒绝提交，也拒绝读缓存**，返回明确错误，
提示用户用 `.remote-compile.toml` 的 `extra_roots` 显式声明。宁可不能用，不可假装能用。

### 6.3 已知盲区（必须写进文档）

`include!` / `env!("CARGO_MANIFEST_DIR")/../..` 读取的仓库外文件、
非 cargo 的外部输入（protoc `-I`、Makefile）。这些只能靠显式 `extra_roots` 兜底。

## 7. 根集合的确定

发现结果经过：git-top 提升（§3）→ 去重 → 排除符号链接根 → anchor 计算。

**嵌套去重必须证明覆盖，不能只看路径包含。** v1 的规则是"落在主根内部就丢弃"。
评审给出的反例成立：主仓库 `.gitignore` 了 `local-crates/`，但被跟踪的 `Cargo.toml`
依赖 `local-crates/foo`。本地 cargo 看得到，metadata 也发现得了，而主根的枚举
（`git ls-files`，`scanner.rs:156`）**不包含被 ignore 的文件**——丢弃之后 `foo` 不在任何
manifest 里，改它也不会改 `root_hash`，缓存命中，构建用的是幽灵代码。

正确规则：只有当外层 manifest **确实包含**内层目录下的条目时才去重；否则保留为独立根
（最长根拥有该子树）。

## 8. 挂载修复（前置依赖）

`docker.rs:203`：

```rust
let mut binds = vec![format!("{}:{}", spec.workspace.display(), spec.workdir)];
```

工作区被挂到 **workdir** 而不是 `/work`。`path = "crates/backend"` 时整个仓库根被别名到
`/work/crates/backend`，真正的子目录变成 `/work/crates/backend/crates/backend`。
（v1 说"cargo 会向上找 workspace 根"是错的——评审纠正：cargo 是**从这个被别名的仓库根开始**的，
所以行为等价于"编整个 workspace，外加一层多余目录"。）
现有测试 `a_subproject_path_becomes_the_working_directory` 只断言字符串拼接，未覆盖挂载。

修法：

```rust
binds.push(format!("{}:/work", spec.workspace.display()));
// working_dir = /work/<anchor_mount>/<profile.path>
```

必须与 §5 的 `executor_abi` 同时上线，否则就是那个 critical 缓存洞。

## 9. 路径安全

评审找到一条**现网已存在**的逃逸路径，多根会放大它：

构建以 worker 的 uid 运行，可以把工作区里的某个目录换成符号链接
（多根下 `/work/private_tun` 这样的挂载点目录尤其显眼）。下一个任务：
- `walk_relative`（`workspace.rs:87`）用 `read_dir` + `file_type()`，是 lstat 语义，
  不会**遍历**进去——这部分是安全的；
- 但 `write_file`（`workspace.rs:138`）的 `root.join(path)` + `std::fs::write`
  **会跟随符号链接**，可以写到工作区之外，例如 worker 的 CAS 或 bundles 目录。

另外 `Root.project_id` / `worktree_id` 来自 agent 且未经格式校验，直接进
`Mirror::open` 的 `root.join(format!("{project_id}.git"))`（`gitmirror.rs:28`）与
`workspace_dir()`，`../../x` 可以逃出各自的父目录。

对策（这几条也应该独立于本提案落地）：

1. 重建前 `lstat` 每一个挂载点及其祖先，是符号链接就删除重建为真目录；
2. 写入走 `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`（或按目录 fd 逐级 `openat` + `O_NOFOLLOW`），
   而不是裸 `fs::write`；
3. `project_id` / `worktree_id` / `mount` 全部按固定格式校验
   （`p-[0-9a-f]{16}` / `w-[0-9a-f]{16}` / 安全相对路径），不合格直接拒绝任务。

## 10. rc-agent

```
resolve_root(path)                       → primary（已是 git top）
adapter.extra_roots(primary)             → 递归 + 提升 + 去重（§6/§7）
complete == false                        → 报错返回，不读缓存、不上传（§6.2）
首见的额外根                              → 阻塞征求同意（§12）
compute_anchor(roots)                    → anchor + 每根 mount
for each root: scan(root, excludes, StatIndex::open(index_path(root)))
合并成一个 anchor 相对的 entries 列表
```

- `StatIndex` 已按根路径分文件（`cfg.index_path(&root)`），天然复用；
- **撕裂防护跨根**（§4.2）：所有根扫完后重新 stat 全部根的文件；任一变动整体重扫。
  评审另指出现有检测只比对**扫描前已知的路径集**（`scanner.rs:92` 的 `stat_all(listing.paths)`），
  扫描过程中**新建**的文件不会被发现——这是既有缺陷，多根下窗口更长，建议一并把
  路径集重新枚举纳入比对；
- `upload_specific`（`engine.rs:433`）现在按 `root.join(entry.path)` 定位文件，
  必须改为按 anchor 定位，否则会去主根下找 private_tun 的文件；
- `worktree_id` 仍由**主根**决定：额外根是构建输入而非独立工作区，共用一个 target volume 正确。

## 11. rc-server / rc-worker

**server**

- `submit` 对每个根 `upsert_project`（供控制台展示），任务表加 `roots_json`；
- `manifest::validate` 追加 mount 校验（安全相对路径、互不为前缀、大小写冲突）；
- **schema 迁移必须真的写**：`migrate()`（`store.rs:237`）只在 `user_version` 落后时
  重跑一遍 `schema.sql`，而 `CREATE TABLE IF NOT EXISTS` 对已存在的表是空操作——
  加列不会生效，随后的 insert/select 全部报错。需要显式事务化 `ALTER TABLE` + 回填 + 版本递增，
  并用"库里存着旧格式排队任务"的场景做重启测试。

**worker**

- 挂载修复（§8）；
- 重建对整个 anchor 跑一次（§4），路径安全按 §9；
- `.rc-state.json` 记录布局指纹，不符则整目录重建。评审正确指出它**不提供原子性**：
  崩溃可留下半棵树或半个 marker。所以 marker 只能当"要不要重建"的提示，
  **绝不能当作"可以跳过重建"的证明**；重建走 staging 目录 → fsync → rename，marker 最后写。
  plan/verify 本来就能自愈，这一条是省重复劳动，不是正确性依赖。

**能力协商**：`EnrollReq` 加 `capabilities`，但**入伍是一次性的**
（`grpc_worker.rs`），升级后的老 worker 不会重新入伍，会被永久排除在多根任务之外。
所以能力必须在 **Channel 建立时**（或心跳里）上报刷新，而不是只在 enroll。

## 12. 隐私（需要用户拍板）

`check ~/code/github/zfc` 会把 `../private_tun` 和 `../shadow-tls-tokio` 一并上传到
**静态不加密**的 CAS（§16）。用户点名一个目录，实际离开机器的是三个。
zfc 本身就有被 git 跟踪的 `private.pem` / `public.pem` / `user_info.json`。

v1 方案是"自动发现 + 结果里披露"。评审的反驳我接受：`engine.rs:145` 的上传发生在
渲染结果之前，**披露发生在传输之后，那不是知情同意，也撤不回来**。

v2：

1. **首次见到某个额外根时阻塞**，返回一条明确的待批准提示（列出路径、体积、文件数），
   不扫描、不上传。用户在 `.remote-compile.toml` 里确认后才放行；
2. ```toml
   extra_roots = ["../private_tun", "../shadow-tls-tokio"]   # 显式白名单
   extra_roots = []                                          # 关闭
   extra_roots = "auto"                                      # 显式选择自动（仍首次确认）
   ```
   **默认不是 auto**：主 VCS 根之外的目录默认需要一次确认；
3. **补 `exclude` 字段**（独立缺口）：目前排除文件的唯一手段是 `.gitignore`，
   被 git 跟踪的密钥文件根本没法排除。

## 13. 失败模式

| 情况 | 行为 |
|---|---|
| 发现不完整 | 拒绝提交，拒绝读缓存，明确报错（§6.2） |
| 首见额外根未批准 | 阻塞并列出待批准清单，不上传（§12） |
| 额外根不是 git 仓库 | ignore-walk 降级 + warning |
| 额外根是符号链接路径 | 拒绝并说明（§3） |
| 额外根嵌套且被外层覆盖 | 去重；未被覆盖则保留为独立根（§7） |
| anchor 深度 > 8 | 拒绝 |
| 无支持多根的 worker | 立即报错，不排队 |

## 14. 评审顺带查出的既有 bug —— **已全部修复**

这些在现网单根路径上就已经存在，与多根无关，已先行修掉（282 测试 + 36 项 smoke 通过）：

1. **【严重】所有 symlink 在 worker 上都是坏的。** ✅
   scanner 把 `hash` 设成 `blake3(target)`，worker 却把 `entry.hash`
   **当 target 原样建链接**，于是 `vendor -> ../shared` 变成
   `vendor -> <64位十六进制串>`，死链。
   一直没被发现是因为 `workspace.rs` 的测试**手工构造**了 `link.hash = "../outside/target"`
   ——真实 scanner 永远产不出这种 manifest，测试把 bug 挡住了。
   修复：`FileEntry` 新增 `symlink_target` 字段（`hash` 继续用于身份/去重）；
   `plan` 只重建**确实不一致**的链接，`verify` 因此也能覆盖 symlink；
   `manifest::validate` 拒绝无 target 或 target 与 hash 不符的条目；
   测试改成按 scanner 的真实产出构造。

2. **【严重】`path` 子项目功能实际不工作。** ✅
   工作区被挂到 `spec.workdir` 而非 `/work`（§8）。改为挂 `WORKSPACE_MOUNT`，
   workdir 单独计算并校验（`../..` 之类直接拒绝）。
   同时加了 `fingerprint::EXECUTOR_ABI`（§5）——**没有它，这个修复本身就是个缓存洞**。

3. **【安全】写入跟随符号链接可以逃出工作区。** ✅
   构建以 worker uid 运行，可把 `src/` 换成指向外部的链接；下一个任务的
   `create_dir_all` + `write` 就跟着写出去了（可写到 worker 的 CAS）。
   修复有两半，缺一不可：
   (a) `plan` 现在区分"想要这个路径"和"这个路径形态正确"——manifest 蕴含的目录
       在磁盘上若是符号链接/普通文件，判为待删；
   (b) **runner 改成先删后写**。原来的顺序是先写后删，(a) 单独存在也拦不住。
   另外 `project_id` / `worktree_id` 现在按固定格式校验（服务端 + worker 双重）。

4. **【健壮性】mirror 与 bundle 没有并发保护。** ✅
   `WorktreeLocks` 只按 worktree 串行，而 mirror 按 **project** 命名，
   同一 project 的两个 worktree 会并发驱动同一个裸库。
   修复：`KeyedLocks` 泛化，新增 project 级锁包住 `Mirror::open` + `ensure_commit` + `extract`；
   bundle 落盘改为临时文件 + 原子 rename。

5. **【潜在】schema 迁移加不了列。** ✅
   `migrate()` 只是重跑 `schema.sql`，而 `CREATE TABLE IF NOT EXISTS` 对已存在的表是空操作。
   改为显式有序 `MIGRATIONS` 步骤 + 事务 + 回滚，并有"给有数据的表加列"和
   "失败不留半个迁移"两个测试。

6. **【潜在】撕裂检测漏掉扫描期间新建的文件。** ✅
   原来只重新 stat **扫描前**已知的路径集，扫描中新建的文件完全不可见。
   改为扫描后重新枚举并按并集比对（新增/消失/改动三种都覆盖）；
   顺带把被排除的路径（`target/`）移出稳定性判据，避免本地构建 churn 造成误报。

> 注意运维影响：`EXECUTOR_ABI` 进入指纹后，**既有缓存结果全部失效**，
> 升级后第一轮任务会真实重编一次。这是有意为之——旧结果是在旧执行语义下算出来的。

## 15. 评审记录

gpt-5.6-sol（reasoning effort: high）对 v1 的裁决为 **REDESIGN**，
4 项 critical、9 项 major。本文采纳的主要意见：

- 扁平 manifest + 额外根仅 L2，取代 v1 的逐根 manifest/mirror/bundle（评审 #13）；
- 指纹加执行器 ABI 维度（#1）；
- 额外根必须提升到 git top-level（#7）；
- 发现失败关闭而非降级缓存（#8）；
- 嵌套去重需证明覆盖（#4）；
- 同意先于上传（#12）；
- 拒绝符号链接根（#6）；
- 能力在 channel 刷新而非仅 enroll、真实 ALTER TABLE 迁移（#9/#10）；
- symlink target、路径逃逸、mirror 竞争等既有 bug 单列（#3/#5/#11）。

评审确认成立、未能攻破的部分：单根 `root_hash` 确实逐字节不变；
`cargo metadata` 确实覆盖 workspace 继承、target-specific 与 artifact path 依赖；
anchor 坐标系本身的相对路径不变量成立。

未采纳：评审建议对 `StatIndex` 的 size+mtime 快路径加 inode/ctime 并定期重哈希（#2）。
该弱点是既有设计的自觉取舍（§4.4），多根只是扩大了影响面，
建议作为独立议题处理，不绑进本提案。

## 16. 工作量

前置条件（§14 的既有 bug、§5 的 ABI 版本、§8 的挂载修复）**已完成**。
剩下的本体集中在 rc-agent（发现、anchor、多根扫描、上传定位）与 rc-worker（多根重建）；
rc-server 只有校验与能力刷新两处——迁移机制已经就位。
proto 只加字段；单根请求的线上形态不变。

## 17. 待用户决策

1. §12 默认策略：默认需确认（**已选定**）；
2. §14 的既有 bug 先单独修掉（**已选定，已完成**）；
3. `exclude` 字段并入还是独立——已确认独立于本提案，尚未实现。
