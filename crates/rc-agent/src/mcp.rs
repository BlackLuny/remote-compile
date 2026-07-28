//! MCP server over stdio (§12).
//!
//! The tool surface is the product: a coding agent should touch one path
//! argument and get back a verdict. Every scan, hash, upload and poll happens
//! here in plain code, where it costs no tokens.

use crate::engine::{CheckRequest, Engine};
use rc_core::model::TaskType;
use rc_core::pb;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const SERVER_NAME: &str = "remote-compile";
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

pub struct McpServer {
    engine: Engine,
}

impl McpServer {
    pub fn new(engine: Engine) -> Self {
        McpServer { engine }
    }

    /// Read newline-delimited JSON-RPC from stdin, write responses to stdout.
    /// stdout carries protocol only — logs go to stderr, or the transport
    /// breaks.
    pub async fn run(&self) -> anyhow::Result<()> {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        let mut stdout = tokio::io::stdout();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let request: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    let err = error_response(Value::Null, -32700, &format!("parse error: {e}"));
                    write_json(&mut stdout, &err).await?;
                    continue;
                }
            };
            if let Some(response) = self.handle(request).await {
                write_json(&mut stdout, &response).await?;
            }
        }
        Ok(())
    }

    /// `None` means the message was a notification, which must not be
    /// answered.
    pub async fn handle(&self, request: Value) -> Option<Value> {
        let method = request.get("method")?.as_str()?.to_string();
        let id = request.get("id").cloned();
        let params = request.get("params").cloned().unwrap_or(json!({}));

        id.as_ref()?;
        let id = id.unwrap_or(Value::Null);

        let result = match method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.call_tool(&params).await,
            "ping" => Ok(json!({})),
            other => Err(McpError::MethodNotFound(other.to_string())),
        };

        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err(McpError::MethodNotFound(m)) => {
                error_response(id, -32601, &format!("method not found: {m}"))
            }
            Err(McpError::InvalidParams(m)) => error_response(id, -32602, &m),
            // A failed build is a valid tool result, not a protocol error: the
            // agent must be able to read it as content. Still goes through the
            // budget gate (R3).
            Err(McpError::Tool(m)) => {
                let text = if self.engine.cfg.budget_gate {
                    rc_core::budget::gate_response(&m)
                } else {
                    m
                };
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": text }], "isError": true }
                })
            }
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let version = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            "instructions": "远程编译检查。改完代码用 check(path) 拿结论；不要在本地跑 cargo check —— \
                             它会把上万行日志灌进上下文。结果分级：结论 → 结构化诊断 → get_log 分页取全量。"
        })
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, McpError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::InvalidParams("tools/call requires `name`".into()))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        let text = match name {
            "check" => self.tool_check(&args).await?,
            "get_result" => self.tool_get_result(&args).await?,
            "get_log" => self.tool_get_log(&args).await?,
            "get_build_profile" => self.tool_build_profile(&args).await?,
            "list_envs" => self.tool_list_envs(&args).await?,
            "prepare_env" => self.tool_prepare_env(&args).await?,
            "get_env_status" => self.tool_env_status(&args).await?,
            "list_workers" => self.tool_list_workers().await?,
            "cancel" => self.tool_cancel(&args).await?,
            other => return Err(McpError::InvalidParams(format!("unknown tool `{other}`"))),
        };
        // Sole hard budget exit for every tool (R3). Errors also go through
        // McpError::Tool → same gate below when re-mapped.
        let text = if self.engine.cfg.budget_gate {
            rc_core::budget::gate_response(&text)
        } else {
            text
        };
        debug_assert!(text.len() <= rc_core::budget::RESPONSE_BUDGET || !self.engine.cfg.budget_gate);
        Ok(json!({ "content": [{ "type": "text", "text": text }] }))
    }

    async fn tool_check(&self, args: &Value) -> Result<String, McpError> {
        let path = required_str(args, "path")?;
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .map(TaskType::parse_or_default)
            .unwrap_or(TaskType::Check);
        let mut env = std::collections::BTreeMap::new();
        if let Some(obj) = args.get("env").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    env.insert(k.clone(), s.to_string());
                }
            }
        }
        let req = CheckRequest {
            path,
            task,
            command: args.get("command").and_then(|v| v.as_str()).map(String::from),
            wait_secs: args.get("wait_secs").and_then(|v| v.as_u64()).map(|v| v as u32),
            no_cache: args.get("no_cache").and_then(|v| v.as_bool()).unwrap_or(false),
            env,
            no_remediate: args.get("no_remediate").and_then(|v| v.as_bool()).unwrap_or(false),
            baseline: args
                .get("baseline")
                .and_then(|v| v.as_str())
                .unwrap_or("auto")
                .to_string(),
        };
        self.engine
            .check(req)
            .await
            .map(|o| o.text)
            .map_err(|e| McpError::Tool(e.to_string()))
    }

    async fn tool_get_result(&self, args: &Value) -> Result<String, McpError> {
        let task_id = required_str(args, "task_id")?;
        let wait = args.get("wait_secs").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let baseline = args
            .get("baseline")
            .and_then(|v| v.as_str())
            .unwrap_or("auto");
        self.engine
            .get_result_with_baseline(&task_id, wait, baseline)
            .await
            .map(|o| o.text)
            .map_err(|e| McpError::Tool(e.to_string()))
    }

    async fn tool_get_log(&self, args: &Value) -> Result<String, McpError> {
        let task_id = required_str(args, "task_id")?;
        // §11 L2: paging is mandatory. An unbounded default would undo the
        // entire point of this system.
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(100).min(1000) as u32;
        let chunk = self
            .engine
            .get_log(pb::LogQuery {
                task_id: task_id.clone(),
                offset: args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0),
                limit,
                grep: args
                    .get("grep")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                tail: args.get("tail").and_then(|v| v.as_bool()).unwrap_or(false),
                ..Default::default()
            })
            .await
            .map_err(|e| McpError::Tool(e.to_string()))?;

        if chunk.total_lines == 0 {
            return Ok("(该任务没有日志：可能是缓存命中，或任务尚未执行)".into());
        }
        let raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);
        let gated = if self.engine.cfg.budget_gate {
            {
                let line_byte_offset = args.get("line_byte_offset").and_then(|v| v.as_u64()).unwrap_or(0);
                // Reserve room for header + pagination footer so final ≤ 8KB.
                let header_reserve = 180;
                rc_core::budget::gate_log_lines(
                    &chunk.lines,
                    chunk.offset + 1,
                    raw,
                    line_byte_offset,
                    header_reserve,
                )
            }
        } else {
            rc_core::budget::BudgetedText {
                text: chunk.lines.join("\n"),
                next_offset: chunk.offset + chunk.lines.len() as u64,
                ..Default::default()
            }
        };
        let next = if gated.next_offset > 0 {
            gated.next_offset
        } else {
            chunk.next_offset.max(chunk.offset + chunk.lines.len() as u64)
        };
        let mut text = format!(
            "lines {}..{} of {}  next_offset={next}  bytes_omitted={}\n",
            chunk.offset,
            chunk.offset + chunk.lines.len() as u64,
            chunk.total_lines,
            gated.bytes_omitted
        );
        text.push_str(&gated.text);
        if chunk.truncated || gated.next_byte_offset > 0 {
            if gated.next_byte_offset > 0 {
                text.push_str(&format!(
                    "\n… 行内续读: get_log(task_id=\"{task_id}\", offset={}, raw=true, line_byte_offset={})",
                    chunk.offset, gated.next_byte_offset
                ));
            } else {
                text.push_str(&format!(
                    "\n… 还有更多；下一页: get_log(task_id=\"{task_id}\", offset={next}, limit={limit})"
                ));
            }
        }
        Ok(text)
    }

    async fn tool_cancel(&self, args: &Value) -> Result<String, McpError> {
        let task_id = required_str(args, "task_id")?;
        self.engine
            .cancel(&task_id)
            .await
            .map_err(|e| McpError::Tool(e.to_string()))
    }

    async fn tool_build_profile(&self, args: &Value) -> Result<String, McpError> {
        let path = required_str(args, "path")?;
        let profile = self
            .engine
            .build_profile(&path)
            .await
            .map_err(|e| McpError::Tool(e.to_string()))?;
        let mut text = String::new();
        if profile.found {
            text.push_str("已有 Build Profile（其他 agent 已摸索出来的，直接用）:\n");
            text.push_str(&profile.config_toml);
            if let Some(h) = &profile.health {
                text.push_str(&format!(
                    "\n健康度: {}/{} 成功，最近成功 {}\n",
                    h.success_count,
                    h.total_count,
                    format_ago(h.last_success_at)
                ));
            }
        } else {
            text.push_str("该项目还没有 Build Profile；check 会用自动探测 + fleet 默认镜像。\n");
        }
        if !profile.resolved_image.is_empty() {
            text.push_str(&format!("镜像: {}\n", profile.resolved_image));
        }
        if !profile.adapter.is_empty() {
            text.push_str(&format!("适配器: {}\n", profile.adapter));
        }
        if !profile.message.is_empty() {
            text.push_str(&profile.message);
            text.push('\n');
        }
        Ok(text)
    }

    async fn tool_list_envs(&self, args: &Value) -> Result<String, McpError> {
        let envs = self
            .engine
            .list_envs(pb::ListEnvsReq {
                query: args.get("query").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                arch: args.get("arch").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                target: args.get("target").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            })
            .await
            .map_err(|e| McpError::Tool(e.to_string()))?;
        if envs.is_empty() {
            return Ok("没有匹配的环境镜像。用 prepare_env(dockerfile=...) 提交一个（异步，管理员审批后生效）。".into());
        }
        let mut text = String::new();
        for e in envs.iter().take(20) {
            let health = e.health.unwrap_or_default();
            text.push_str(&format!(
                "{}  [{}]  成功率 {:.0}% / {} 次  最近成功 {}\n  {}\n",
                e.image_ref,
                e.status,
                health.success_rate_7d * 100.0,
                health.total_runs,
                format_ago(health.last_success_at),
                if e.description.is_empty() { "-" } else { &e.description }
            ));
        }
        Ok(text)
    }

    async fn tool_prepare_env(&self, args: &Value) -> Result<String, McpError> {
        let dockerfile = args.get("dockerfile").and_then(|v| v.as_str()).unwrap_or_default();
        let image = args.get("image").and_then(|v| v.as_str()).unwrap_or_default();
        if dockerfile.is_empty() && image.is_empty() {
            return Err(McpError::InvalidParams(
                "prepare_env 需要 dockerfile 或 image 之一".into(),
            ));
        }
        let status = self
            .engine
            .prepare_env(pb::PrepareEnvReq {
                dockerfile: dockerfile.to_string(),
                image_ref: image.to_string(),
                project_id: args.get("project").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                reason: args.get("reason").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                agent_session: String::new(),
                description: args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
            .await
            .map_err(|e| McpError::Tool(e.to_string()))?;
        Ok(format!(
            "env_id={}  status={}\n{}\n这是异步的：继续写你的代码，用 get_env_status(env_id) 查询进度。",
            status.env_id, status.status, status.message
        ))
    }

    async fn tool_env_status(&self, args: &Value) -> Result<String, McpError> {
        let env_id = required_str(args, "env_id")?;
        let status = self
            .engine
            .env_status(&env_id)
            .await
            .map_err(|e| McpError::Tool(e.to_string()))?;
        let health = status.health.unwrap_or_default();
        Ok(format!(
            "env_id={}  status={}\n{}\n镜像: {}\n最近成功: {}  成功率 {:.0}% / {} 次",
            status.env_id,
            status.status,
            status.message,
            if status.image_ref.is_empty() { "(尚未构建)" } else { &status.image_ref },
            format_ago(health.last_success_at),
            health.success_rate_7d * 100.0,
            health.total_runs
        ))
    }

    async fn tool_list_workers(&self) -> Result<String, McpError> {
        let resp = self
            .engine
            .list_workers()
            .await
            .map_err(|e| McpError::Tool(e.to_string()))?;
        let mut text = format!(
            "worker {} 台在线；队列深度 {}，运行中 {}\n",
            resp.workers.iter().filter(|w| w.status == "online").count(),
            resp.queue_depth,
            resp.running
        );
        for w in resp.workers.iter().take(30) {
            text.push_str(&format!(
                "{}  {}  cpu {:.0}%  disk {}GB  {}/{} 任务\n",
                w.worker_id,
                w.status,
                w.cpu_load * 100.0,
                w.disk_free_gb,
                w.running_tasks,
                w.max_parallel
            ));
        }
        Ok(text)
    }
}

pub enum McpError {
    MethodNotFound(String),
    InvalidParams(String),
    Tool(String),
}

fn required_str(args: &Value, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| McpError::InvalidParams(format!("missing required argument `{key}`")))
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn write_json(out: &mut tokio::io::Stdout, value: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

fn format_ago(ts: i64) -> String {
    if ts <= 0 {
        return "从未".into();
    }
    let delta = rc_core::now_secs() - ts;
    match delta {
        d if d < 60 => "刚刚".into(),
        d if d < 3600 => format!("{} 分钟前", d / 60),
        d if d < 86400 => format!("{} 小时前", d / 3600),
        d => format!("{} 天前", d / 86400),
    }
}

/// §12. Descriptions are written for the agent that will read them: what the
/// tool does, and when *not* to reach for something else.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "check",
            "description": "远程编译检查当前工作区。默认短等待，多数增量 check 直接同步返回结论；\
                            超时则返回 task_id 转异步。返回结论 + 结构化诊断，不返回原始日志。\
                            改完代码用这个，不要本地跑 cargo check。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "工作区内任意路径；会自动上溯到仓库根" },
                    "task": { "type": "string", "enum": ["check", "build", "test", "clippy"], "default": "check" },
                    "command": { "type": "string", "description": "覆盖默认命令（少用；优先写进 .remote-compile.toml）" },
                    "wait_secs": { "type": "integer", "description": "同步等待秒数，默认 4" },
                    "no_cache": { "type": "boolean", "description": "跳过指纹缓存强制重编，默认 false" },
                    "env": { "type": "object", "description": "请求级环境变量（分层叠加；有 denylist）", "additionalProperties": { "type": "string" } },
                    "no_remediate": { "type": "boolean", "description": "关闭 OOM 自动降配重试，默认 false" },
                    "baseline": { "type": "string", "description": "诊断增量基线：auto|none|last_success|<task_id>，默认 auto" }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "get_result",
            "description": "轮询一个异步任务的结果。token 成本极低，可以反复调用。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "wait_secs": { "type": "integer", "description": "长轮询等待秒数，默认 0（立即返回）" }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "cancel",
            "description": "取消仍在执行的任务（镜像管理端 cancel 路径）。",
            "inputSchema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "get_log",
            "description": "分页取全量构建日志。必须带 limit/grep —— 完整日志动辄上万行，\
                            全量拉取会挤爆上下文。定位问题优先用 grep=\"error\"。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "offset": { "type": "integer", "default": 0 },
                    "limit": { "type": "integer", "default": 100, "maximum": 1000 },
                    "grep": { "type": "string", "description": "只返回包含该子串的行（不区分大小写）" },
                    "tail": { "type": "boolean", "description": "从末尾取，默认 false" },
                    "raw": { "type": "boolean", "description": "跳过单行省略，仍受响应总上限约束" },
                    "line_byte_offset": { "type": "integer", "description": "raw 模式下单行内续读的字节偏移" }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "get_build_profile",
            "description": "查这个项目的构建档案：命令、镜像、健康度。进入一个新项目时先问一句，\
                            能省掉自己摸索构建方式的来回。",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        }),
        json!({
            "name": "list_envs",
            "description": "按关键字搜索现成的编译环境镜像及其健康度。缺依赖时先搜一下，\
                            别人趟过的坑不用再趟。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "如 \"rust protoc\"" },
                    "arch": { "type": "string" },
                    "target": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "prepare_env",
            "description": "提交一个新的编译环境（Dockerfile 或上游镜像）。永远异步返回，\
                            绝不阻塞你继续写代码；新镜像需要管理员审批后才能执行代码。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dockerfile": { "type": "string" },
                    "image": { "type": "string", "description": "或者直接引用一个上游镜像" },
                    "project": { "type": "string" },
                    "reason": { "type": "string", "description": "为什么需要它 —— 审批的人要看" },
                    "description": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "get_env_status",
            "description": "查询环境镜像的构建进度与健康度。",
            "inputSchema": {
                "type": "object",
                "properties": { "env_id": { "type": "string" } },
                "required": ["env_id"]
            }
        }),
        json!({
            "name": "list_workers",
            "description": "编译资源池概况。诊断用：任务排队久了看看是不是没有 worker。",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    fn server() -> McpServer {
        McpServer::new(Engine::new(AgentConfig {
            // Nothing is listening here; tool calls fail with the local
            // fallback message, which is exactly what we want to assert.
            server: "http://127.0.0.1:1".into(),
            ..Default::default()
        }))
    }

    #[tokio::test]
    async fn initialize_echoes_the_clients_protocol_version() {
        let resp = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn initialize_falls_back_to_a_known_version() {
        let resp = server()
            .handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn notifications_are_never_answered() {
        // Replying to a notification is a protocol violation that some clients
        // treat as a hard error.
        assert!(server()
            .handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn tools_list_matches_the_documented_surface() {
        let resp = server()
            .handle(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        let names: Vec<String> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        // §12 table (+ cancel for mechanism five).
        for expected in [
            "check",
            "get_result",
            "cancel",
            "get_log",
            "get_build_profile",
            "list_envs",
            "prepare_env",
            "get_env_status",
            "list_workers",
        ] {
            assert!(names.contains(&expected.to_string()), "missing tool {expected}");
        }
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn every_tool_declares_a_usable_schema() {
        for tool in tool_definitions() {
            let name = tool["name"].as_str().unwrap();
            assert!(!tool["description"].as_str().unwrap().is_empty(), "{name}");
            assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        }
    }

    #[test]
    fn check_takes_only_a_path() {
        // §1.1: the agent's input surface is one argument.
        let check = tool_definitions()
            .into_iter()
            .find(|t| t["name"] == "check")
            .unwrap();
        assert_eq!(check["inputSchema"]["required"], json!(["path"]));
    }

    #[test]
    fn get_log_defaults_to_a_bounded_page() {
        let tool = tool_definitions()
            .into_iter()
            .find(|t| t["name"] == "get_log")
            .unwrap();
        let props = &tool["inputSchema"]["properties"];
        assert_eq!(props["limit"]["default"], 100);
        assert_eq!(props["limit"]["maximum"], 1000);
    }

    #[tokio::test]
    async fn an_unknown_method_returns_a_jsonrpc_error() {
        let resp = server()
            .handle(json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn a_missing_required_argument_is_an_invalid_params_error() {
        let resp = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "check", "arguments": {} }
            }))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert!(resp["error"]["message"].as_str().unwrap().contains("path"));
    }

    #[tokio::test]
    async fn an_unknown_tool_is_reported_as_invalid_params() {
        let resp = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": { "name": "nope", "arguments": {} }
            }))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_is_a_tool_result_not_a_protocol_error() {
        // The agent must be able to *read* the advice to run cargo check
        // locally; a JSON-RPC error would be swallowed by most clients.
        let resp = server()
            .handle(json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": { "name": "list_workers", "arguments": {} }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("cargo check"), "{text}");
    }

    #[tokio::test]
    async fn ping_is_answered() {
        let resp = server()
            .handle(json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" }))
            .await
            .unwrap();
        assert!(resp["result"].is_object());
        assert_eq!(resp["id"], 7);
    }

    #[test]
    fn relative_times_read_naturally() {
        assert_eq!(format_ago(0), "从未");
        let now = rc_core::now_secs();
        assert_eq!(format_ago(now - 30), "刚刚");
        assert_eq!(format_ago(now - 600), "10 分钟前");
        assert_eq!(format_ago(now - 7200), "2 小时前");
        assert_eq!(format_ago(now - 3 * 86400), "3 天前");
    }
}
