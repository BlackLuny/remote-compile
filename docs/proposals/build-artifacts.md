# 构建产物回传（build artifacts）v1

状态：**已实现，未上线**（尚无真机端到端验证）。
关联：§1.1、§3.2、§3.5、§5.1、§7.1、§9、§11、§12、§13、§18。

## 1. 问题

remote-compile 的出发点是"本地零产物"（§1），`build` 任务跑完后二进制留在 worker 的
target volume 里，开发机拿不到。需要产物的场景：

- 部署 / 测试到 Linux 机器（worker 只有 Linux，产物是 Linux ELF，**在 mac 上不能运行**）；
- 交叉编译产物（musl 静态、wasm）、库（`.so`/`.a`）；
- 构建生成的文件（codegen 输出等）。

## 2. 目标与非目标

**目标**

1. 显式声明才回传，默认行为逐字节不变；
2. 产物内容绝不进入 MCP 返回（§1.1 token 经济），结果只带清单；
3. 不可信构建代码不能借"产物"读到 worker host 上的任何文件；
4. 大文件不撑爆控制面内存。

**非目标**：产物签名 / 供应链证明；跨任务增量传输（二进制每次都变，CDC 分块收益低）；
mac/windows 可运行产物（§1.2）。

## 3. 声明

`.remote-compile.toml` / BuildProfile：

```toml
[artifacts]
paths = ["target/release/rc-worker", "dist/*.wasm"]   # 相对子项目目录；target/ 映射到 CARGO_TARGET_DIR
auto = true          # Rust：从 compiler-artifact JSON 取 workspace 成员的 executable
max_total_mb = 512   # 单任务上限，默认 512
```

`check` 工具新增 `artifacts` 参数（字符串数组，或 `["auto"]`）作单次覆盖。只有
`build` / `custom` 任务收集产物；其余任务类型忽略声明并在结果里提示。

产物声明进入 fingerprint（§5.1）：否则"先无声明 build、再带声明 build"会命中缓存拿到
空产物。

## 4. 收集：只在容器内解引用

target 位于 docker volume，内容由不可信 build.rs 写入。worker 以 root 在 host 上直接读
volume 路径时，一个指向 `/etc/shadow` 或 worker 凭据的 symlink 就会被当作产物回传。

因此：

1. worker 为每个任务创建空 host 临时目录，bind 到容器 `/rc/out`（可写）；
2. 构建成功后，用同一镜像、同一 target volume 起**第二个**短命沙箱（断网、300s 超时）
   跑收集脚本：`cp -L` 声明路径到 `/rc/out/<相对路径>`，解引用发生在容器命名空间内。
   不追加在构建脚本末尾，是因为 `auto` 模式要先解析构建的 JSON 输出；pattern 经只读挂载的
   列表文件传入，不拼进 shell；
3. worker 读 `/rc/out` 时只接受普通文件（`symlink_metadata`，拒绝 symlink / 设备 /
   FIFO），路径必须规范化后仍位于 `/rc/out` 内，校验文件数（≤256）与总大小；超限截断并
   在 `artifacts_note` 说明，不改变任务 kind；
4. 仅 `kind == success` 时上传。

`auto` 模式：worker 解析 stdout 的 `compiler-artifact` 消息，取 `executable` 非空且
`package_id` 为 path 源（非 registry / git）的条目，容器路径 `/rc/target/...` 映射回
收集清单。

## 5. 传输

- 每个产物 zstd 压缩后作为独立 CAS blob 上传。
- `PutBlob`（worker→server）改为流式：边收边写临时文件、边算 blake3，结束校验后 rename
  入 CAS；不再整块读入内存。worker 端边读文件边发 chunk。
- `AgentApi.FetchArtifact(task_id, path) returns (stream BlobChunk)`：按 task 查表取 hash，
  不开放任意 hash 读取（避免成为跨项目读 CAS 的口子）。

proto：

```proto
message Artifact {
  string path = 1;            // 相对产物根的路径
  string blob = 2;            // zstd 压缩后的 CAS hash
  uint64 size = 3;            // 解压后大小
  uint64 compressed_size = 4;
  uint32 mode = 5;            // 可执行位
  string content_hash = 6;    // 解压后 blake3，agent 落盘校验
}
TaskResult { repeated Artifact artifacts = 16; string artifacts_note = 17; string artifact_target = 18; }
```

`artifact_target` 记录 target triple 与镜像，提醒产物只在对应平台可运行。

## 6. 存储与 GC

新表：

```sql
CREATE TABLE task_artifacts (task_id TEXT, path TEXT, hash TEXT, size INT,
                             expires_at INT, PRIMARY KEY (task_id, path));
```

产物 blob **不**写入 `task_blob_refs`（否则 task 行在即永不回收），由 `task_artifacts`
独立持有，`expires_at = now + artifact_ttl`（默认 24h，可配）。GC：先删过期
`task_artifacts` 行，blob 若再无任何引用则随普通 CAS GC 回收（`collectable_blobs` 增加
`NOT EXISTS task_artifacts` 条件）。

缓存命中时若任一产物已过期，按 miss 处理重跑。

## 7. agent 侧

- 声明了产物的项目跳过 rc-agent 本地结果缓存（本地结论可能比产物活得久）；
- CLI：`rc-agent check <path> --task build --artifact auto --out ./dist`；
- `check` 结果尾部附清单：`产物 2 个 (18.4MB)：rc-worker, rc-agent → fetch_artifacts(task_id=…)`；
- 新 MCP 工具 `fetch_artifacts(task_id, paths?, dest?)`：下载、解压、校验 content_hash、
  恢复可执行位，写入 `dest`（默认 `<项目根>/target/remote/<task_id>/`；`target/` 已被
  扫描排除，§4.3），只返回写入路径与大小；
- `dest` 不得逃出项目根；产物内路径含 `..` / 绝对路径直接拒绝。

## 8. 落地顺序

1. 流式 PutBlob；
2. proto / 表 / GC；
3. worker `/rc/out` 收集（paths，再 auto）；
4. fingerprint 与缓存命中失效；
5. agent `FetchArtifact` + `fetch_artifacts`。

## 9. 评审修订

一轮独立评审（2 High / 4 Medium）后的修订：

- **收集步骤可写满 host 磁盘**：build.rs 把产物做成指向 `/dev/zero` 的链接，`stat` 报 0
  字节，`cp -L` 无限写入 host bind 目录。修订：只收普通文件（`-f`），用
  `head -c 剩余预算+1` 限长复制，`PATH` 钉死为镜像系统目录（target/registry volume 可被
  构建写入）；worker 侧另有看门狗每 250ms 统计 `/rc/out`，超预算 16MB 即 kill 容器。
- **glob 可展开出 `..`**（`.?`、`[.].`）：脚本跳过含 `.`/`..` 段的匹配；`auto` 路径拒绝 glob
  字符与控制字符。
- **GC 与上传竞态**：同内容（可复现构建）的旧行过期时，新任务刚上传、尚未落行的 blob 会被删。
  修订：`last_used` 在 1h 宽限期内的 blob 连同其过期行一起推迟到下一轮。
- **瞬时失败被缓存**：上传失败 / blob 丢失时结果置 `artifacts_incomplete`，该结果永不
  作为缓存命中返回；服务端落库前剔除不可取的产物并写入 note。
- **note 无上限**：脚本最多输出 20 条，worker/server 统一 `cap_notes`，避免 TaskDone 超过
  gRPC 4MB。
- `PutBlob` 的写盘与 fsync 移出 async 线程；agent 下载以声明大小为解压上限，取消时由
  drop guard 清掉半截文件；scratch 目录清理前先收回权限。
