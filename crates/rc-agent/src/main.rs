//! rc-agent — the local MCP server a coding agent talks to.
//!
//! It is deliberately the only piece an agent sees: one `check(path)` call in,
//! a verdict out. Scanning, hashing, uploading and polling all happen here in
//! code, where they cost no tokens (§1.1).

mod client;
mod config;
mod consent;
mod engine;
mod excludes;
mod index;
mod mcp;
mod multiroot;
mod scanner;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::AgentConfig;
use engine::{CheckRequest, Engine};
use rc_core::model::TaskType;

#[derive(Parser)]
#[command(name = "rc-agent", version, about = "remote-compile MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve MCP over stdio. This is what a coding agent launches.
    Serve,
    /// Point this agent at a control plane and store its token.
    Configure {
        #[arg(long, env = "RC_SERVER")]
        server: Option<String>,
        #[arg(long, env = "RC_AGENT_TOKEN")]
        token: Option<String>,
        /// Reset the session id. Rarely wanted: it also resets supersede scope.
        #[arg(long)]
        new_session: bool,
    },
    /// Show the current configuration.
    Status,
    /// Run one check from a terminal — handy for verifying a setup.
    Check {
        path: String,
        #[arg(long, default_value = "check")]
        task: String,
        #[arg(long)]
        wait_secs: Option<u32>,
        #[arg(long)]
        no_cache: bool,
        /// Override the default command (same as MCP `command`).
        #[arg(long)]
        command: Option<String>,
    },
}

fn main() -> Result<()> {
    // stdout is the MCP transport; every log line must go to stderr or the
    // protocol breaks.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rc_agent=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            let cfg = AgentConfig::load_or_create()?;
            cfg.ensure_dirs()?;
            tracing::info!(server = %cfg.server, session = %cfg.agent_session, "rc-agent MCP server ready");
            rt.block_on(mcp::McpServer::new(Engine::new(cfg)).run())
        }
        Command::Configure { server, token, new_session } => {
            let mut cfg = AgentConfig::load_or_create()?;
            if let Some(s) = server {
                cfg.server = s;
            }
            if let Some(t) = token {
                cfg.token = t;
            }
            if new_session {
                cfg.agent_session = rc_core::ids::agent_session_id();
            }
            cfg.save()?;
            println!("configuration written to {}", AgentConfig::config_path().display());
            println!("server:  {}", cfg.server);
            println!("session: {}", cfg.agent_session);
            println!("token:   {}", if cfg.token.is_empty() { "(none)" } else { "(set)" });
            Ok(())
        }
        Command::Status => {
            let cfg = AgentConfig::load_or_create()?;
            let ver = env!("CARGO_PKG_VERSION");
            let sha = option_env!("VERGEN_GIT_SHA")
                .or(option_env!("GIT_SHA"))
                .unwrap_or("unknown");
            println!("version: {ver}+{sha}");
            println!("config:  {}", AgentConfig::config_path().display());
            println!("cache:   {}", cfg.cache_root().display());
            println!("server:  {}", cfg.server);
            println!("session: {}", cfg.agent_session);
            println!("token:   {}", if cfg.token.is_empty() { "(none)" } else { "(set)" });
            Ok(())
        }
        Command::Check {
            path,
            task,
            wait_secs,
            no_cache,
            command,
        } => {
            let cfg = AgentConfig::load_or_create()?;
            let engine = Engine::new(cfg);
            let outcome = rt.block_on(engine.check(CheckRequest {
                path,
                task: TaskType::parse_or_default(&task),
                command,
                wait_secs,
                no_cache,
                env: Default::default(),
                no_remediate: false,
                baseline: "auto".into(),
            }))?;
            println!("{}", outcome.text);
            // Some outcomes never became a task at all — nothing was submitted,
            // so there is nothing to poll and no task id to offer.
            let no_task = outcome.task_id.is_empty();
            if !no_task && !rc_core::TaskState::parse_or_default(&outcome.status).is_terminal() {
                eprintln!(
                    "still running (status={}); poll with: rc-agent check --wait-secs 60, or get_result(task_id=\"{}\")",
                    outcome.status, outcome.task_id
                );
            }
            // A failed build is a failed command, so scripts can branch on it.
            if matches!(
                outcome.kind,
                Some(rc_core::ResultKind::CompileError)
                    | Some(rc_core::ResultKind::EnvError)
                    | Some(rc_core::ResultKind::Timeout)
            ) {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
