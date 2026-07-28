# Intent / Query Surface 二轮实现检视

## 总判

**APPROVE_WITH_CHANGES**

## 遗留问题

### R2-F1 — MAJOR — cache hit 的 `EffectivePlan.path` 仍可能缺失

`crates/rc-server/src/app.rs:395-404` 只在缓存结果完全没有
`effective_plan` 时补写权威 plan；若旧缓存已有 `Some(EffectivePlan)`、但
`path=None`（正是修复前 worker 产生的结果），该分支不会修复 path。

同时，`crates/rc-server/src/store.rs:846-859` 的 `record_cache_hit` 仍把源
`result_json` 原样复制到新 task row；`app.rs` 中对返回值的内存补写没有持久化。
因此即使 `effective_plan=None` 的即时 cache-hit 响应被补齐，后续按新 task_id
轮询仍可能读到未补齐的 Receipt。此行为仍不满足 §5.2“每次终态结果（含 cache
hit）必显准确 scope”的要求。

建议在复制前以本次 server 权威 `resolved_cmd` 无条件合并
`EffectivePlan.path/command/command_is_default/scope_hash`，并将合并后的
`result_json` 持久化到 cache-hit task row；补覆盖“旧结果 plan.path=None”及
cache hit 后再次轮询的回归测试。

### R2-F2 — MAJOR — `resolve_image` 仍接受规范外 suffix / substring

`crates/rc-server/src/images.rs:91-95` 接受任意
`refer.ends_with(row.digest)`；`images.rs:108-120` 又接受
`row.id.ends_with(refer)` 以及 `image_ref` 路径内的 `contains` 命中。这些都不是
§6.2 定义的精确 env_id、完整 image reference、repository+tag 或唯一 env_id short
prefix，唯一命中时仍会 silent pick。

例如带任意前缀但以真实 digest 结尾的字符串、env_id 的后缀、或碰巧出现在
`image_ref` 路径段中的 8 字符串，均可能被解析为单一镜像。应删除这些
`ends_with`/`contains` 分支，只保留精确引用与 `row.id.starts_with(refer)` 的唯一
前缀匹配，并补 substring/suffix 必须 `not_found` 的回归测试。

## 验证

`cargo test -p rc-core -p rc-server -p rc-agent -p rc-worker`：538 passed，0 failed。
