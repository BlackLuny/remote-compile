---
name: add-rc-worker
description: Add a new rc-worker (compile machine) to the remote-compile fleet, or reinstall one, end to end — preflight the host, install the binary production actually runs, enroll, give it the image-registry credential, and prove it takes real tasks. Use when the user says "加一台 worker", "新增编译机", "把 X 机器加进编译池", "add a worker", "enroll a worker", or hands over a host (ip:port) to become a compile machine. Also use when a worker is online but never gets tasks.
---

# 添加 rc-worker

每一步都是之前真实踩过的坑换来的。按顺序做，**每步验证通过再往下走**。

控制面：rc 机器 `ssh -p 24946 root@192.99.148.123`（以下写作 `RC`）。
对外入口 `https://build.coderluny.com`（Caddy：gRPC 分流给 7701，`/v2/*` 是镜像仓库）。

## 0. 先确认的事（问用户，不要自己决定）

- 目标主机 SSH（`host:port`，root）。本机 `~/.ssh/config` 里常已有条目，先 `grep` 一下。
- **这台机器上还跑着什么。** rc-worker 持有 docker socket（等于 root），执行不可信的
  build.rs。与其他业务混部是用户的风险决策——说清楚，由用户拍板，不要默认接受。
- `max_parallel`（默认 2）。

## 1. 预检目标主机（任一不过就停下报告）

```bash
ssh <host> 'uname -sm; ldd --version | head -1; docker info --format "{{.ServerVersion}}";
  df -h /var/lib/docker /var/lib 2>/dev/null; nproc; free -g | head -2;
  curl -s -o /dev/null -w "registry=%{http_code}\n" https://build.coderluny.com/v2/;
  id rc-worker 2>&1; systemctl is-active rc-worker 2>&1'
```

| 检查 | 要求 | 为什么 |
|---|---|---|
| `uname -sm` | `Linux x86_64` | 生产二进制只有 x86_64；aarch64 需要单独编一份 |
| glibc | **≥ 2.34**（Debian 12+/Ubuntu 22.04+） | 生产二进制在 Debian 12 上编，需要 `GLIBC_2.34` |
| docker | daemon 可达 | 不装 docker 就什么都跑不了 |
| 磁盘可用 | **≥ 50G**，绝不能 < 20G | 调度器低于 20G 直接跳过（yxvm 96% 满，从没接过任务）；一个编译镜像约 3G，每个 worktree 的 target 缓存几十 G |
| registry | 返回 `401` | 能到控制面 443；200 说明 Caddy 没加认证，停下 |
| rc-worker 用户/服务 | 新机器应不存在 | 已存在 = 重装，见 §6 |

## 2. 用生产正在跑的二进制，不要用 GitHub Release

GitHub latest release 可能**落后于生产**（v0.1.2 时就缺产物回传、镜像上报、自动拉镜像）。
旧 worker 会被调度器因缺 capability 排除，或者静默少功能。从 rc 机器取：

```bash
S=<scratchpad>
scp -P 24946 root@192.99.148.123:/usr/local/bin/rc-worker $S/rc-worker
shasum -a 256 $S/rc-worker; ssh -p 24946 root@192.99.148.123 sha256sum /usr/local/bin/rc-worker   # 必须相同
ssh <host> 'mkdir -p /root/rc-install'
scp $S/rc-worker <host>:/root/rc-install/rc-worker
scp deploy/worker-install.sh <host>:/root/rc-install/
ssh <host> 'cd /root/rc-install && sha256sum rc-worker && ./rc-worker --version'   # 能运行才继续
```

安装脚本看到同目录 `./rc-worker` 就直接用它，**且不会自己校验能否运行**——所以上面的
`--version` 必须先过。

## 3. 签一次性 enrollment token 并安装

```bash
TOKEN=$(ssh -p 24946 root@192.99.148.123 \
  'runuser -u rc-server -- /usr/local/bin/rc-server --data-dir /var/lib/rc-server enroll-token --ttl-secs 1800 2>/dev/null')
ssh <host> "cd /root/rc-install && RC_SERVER=https://build.coderluny.com RC_ENROLLMENT_TOKEN=$TOKEN \
  RC_MAX_PARALLEL=2 bash ./worker-install.sh"
unset TOKEN
```

- `RC_SERVER` 用 **`https://build.coderluny.com`**（Caddy 按 `application/grpc` 分流）。只有 rc
  机器本机的 worker 用 `http://127.0.0.1:7701`。
- 必须用 **`bash`** 跑：脚本用了 bash 数组，Debian 的 `sh` 是 dash，会在第 68 行语法报错。
- token 单次有效；失败重来就重新签一个。
- 必须以 `rc-server` 用户跑 `enroll-token`：以 root 跑会在库目录里留下 root 属主文件。

## 4. 镜像仓库凭据（漏了它，这台 worker 对所有项目都没用）

编译环境镜像是编译池自己构建的 `rc-registry/env/…`，只存在于编译池仓库。worker 缺镜像时
从 `build.coderluny.com` 拉取，凭据只认 `/var/lib/rc-worker/.docker/config.json`：

```bash
ssh -p 24946 root@192.99.148.123 '. /root/.rc-registry-credentials;
  printf "{\"auths\":{\"build.coderluny.com\":{\"auth\":\"%s\"}}}" "$(printf "%s:%s" "$user" "$pass" | base64 -w0)"' \
| ssh <host> 'install -d -m 0700 -o rc-worker -g "$(id -gn rc-worker)" /var/lib/rc-worker/.docker;
  umask 077; cat > /var/lib/rc-worker/.docker/config.json; chown rc-worker: /var/lib/rc-worker/.docker/config.json;
  stat -c "%U %a %s" /var/lib/rc-worker/.docker/config.json'
```

期望 `rc-worker 600 121` 左右。**凭据只经管道传递，不要 echo、不要写进命令行参数或日志。**
`/root/.docker/config.json` 没用——worker 以 rc-worker 用户运行，读不到。

## 5. 验证它真的能干活（"online" 不等于能接任务）

1. **连上控制面**：
   ```bash
   ssh <host> 'systemctl is-active rc-worker; journalctl -u rc-worker --since "-2min" --no-pager | grep -E "channel open|ERROR"'
   ssh -p 24946 root@192.99.148.123 'journalctl -u rc-server --since "-5min" --no-pager | grep "worker channel opened"'
   ```
   记下新 worker_id（在目标机 `/var/lib/rc-worker/worker.json`；读它时**只打印 `worker_id`**，
   别把 `worker_token` 打出来）。
2. **能拉镜像**（先于真任务单独验，排障更干净）：
   ```bash
   ssh <host> 'runuser -u rc-worker -- docker --config /var/lib/rc-worker/.docker pull build.coderluny.com/rc-env:0f5446c3 >/dev/null && docker image inspect build.coderluny.com/rc-env:0f5446c3 --format "{{.Id}}"'
   ```
   Id 应等于 zfc 钉住的 `sha256:d5b6c528…`。（这一步顺带预热，可选。）
3. **真接到任务**：调度器偏向已有 worktree 缓存和镜像的机器，新机器闲着不代表坏了。同一
   worktree 在一台 worker 上串行，所以并发提交同一仓库的两个 check，第二个会被挤到别的机器：
   ```bash
   cd ~/code/github/zfc
   rc-agent check zf-worker/crates/zfw-runtime --no-cache --wait-secs 1800 &
   sleep 3; rc-agent check forwarder-server --no-cache --wait-secs 1800 & wait
   ssh -p 24946 root@192.99.148.123 "sqlite3 -readonly /var/lib/rc-server/rc-server.sqlite \
     \"SELECT substr(task_id,-4), phase, worker_id, substr(detail,1,100) FROM task_events
       WHERE task_id IN (SELECT id FROM tasks ORDER BY created_at DESC LIMIT 2) ORDER BY at_ms;\""
   ```
   看到新 worker_id 出现在 `dispatched` 且 `finished … success`。三台以上时可能仍落在老机器上，
   多提交几个不同 crate 即可。
4. **排查"在线但从不接任务"**：先看任务事件里的 `infra_retry` 明细——
   `pull rc-registry/env/…` = 凭据缺失或镜像没发布；否则查磁盘（< 20G 被跳过）和版本
   （旧二进制缺 capability）。历史上 14 天 0 任务就是镜像拉不到被静默重试回 rc 机器。

## 6. 重装 / 升级 / 下线

- **升级二进制**（已 enroll）：按 §2 取生产二进制，确认整个编译池无活跃任务后
  `install -m 0755` 覆盖并 `systemctl restart rc-worker`；旧的备份为
  `/usr/local/bin/rc-worker.bak-<日期>`。**所有 worker 一起升级**，只升 rc 机器那台是踩过的坑。
- **重装**：`worker.json` 存在时安装脚本保留身份、只换二进制，不需要 token。
- **下线**：控制台 Drain 等任务跑完 → `rc-worker uninstall --yes && rm -f /usr/local/bin/rc-worker`。

## 7. 收尾

- 删除目标机 `/root/rc-install/` 和本地 scratch 里的二进制。
- 更新记忆里的 worker 清单（`build-artifacts-deployed.md`：worker_id → 主机、SSH、磁盘）。
- 向用户报告：worker_id、主机、磁盘余量、验证到的任务 id，以及未验证项。
