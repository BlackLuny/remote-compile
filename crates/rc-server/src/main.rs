//! rc-server — the remote-compile control plane.
//!
//! One process serves three surfaces: gRPC for agents and workers, a JSON
//! admin API, and the embedded console SPA.

mod admin;
mod app;
mod assets;
mod auth;
mod bg;
mod config;
mod events;
mod grpc_agent;
mod grpc_worker;
mod images;
mod metrics;
mod scheduler;
mod store;
mod workers;

use anyhow::{Context, Result};
use app::App;
use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rc-server", version, about = "remote-compile control plane")]
struct Cli {
    /// Where SQLite, the CAS and logs live.
    #[arg(long, env = "RC_DATA_DIR", default_value = "./rc-data", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane.
    Serve {
        /// Admin console + REST API + /metrics.
        #[arg(long, env = "RC_HTTP_ADDR", default_value = "0.0.0.0:7700")]
        http_addr: String,
        /// gRPC endpoint for agents and workers.
        #[arg(long, env = "RC_GRPC_ADDR", default_value = "0.0.0.0:7701")]
        grpc_addr: String,
        /// Skip agent token checks. Single-user local experiments only.
        #[arg(long, env = "RC_ALLOW_ANONYMOUS_AGENTS")]
        allow_anonymous_agents: bool,
        #[arg(long, default_value_t = 7 * 24 * 3600)]
        session_ttl_secs: i64,
    },
    /// Create or reset a console account.
    Admin {
        #[arg(long)]
        username: String,
        #[arg(long, env = "RC_ADMIN_PASSWORD")]
        password: String,
        #[arg(long, default_value = "admin")]
        role: String,
    },
    /// Mint an agent token (what `rc-agent` puts in its config).
    AgentToken {
        #[arg(long, default_value = "default")]
        label: String,
    },
    /// Mint a single-use worker enrollment token.
    EnrollToken {
        #[arg(long, default_value_t = 3600)]
        ttl_secs: i64,
    },
    /// Approve an image digest without opening the console.
    ApproveImage {
        env_id: String,
        #[arg(long, default_value = "cli")]
        by: String,
    },
    /// Approve (or revoke) one egress host for one project (§7.1).
    ApproveEgress {
        project_id: String,
        host: String,
        /// Take the approval back. The row stays for the audit trail; the next
        /// task dispatched simply stops carrying the host.
        #[arg(long)]
        revoke: bool,
        #[arg(long, default_value = "cli")]
        by: String,
    },
    /// List the `pre_commands` scripts agents have learned and offered to the
    /// fleet, so there is something to read before approving one (§3.2).
    ListPreCommands {
        /// `pending_approval` | `approved` | `rejected` | `superseded`.
        #[arg(long)]
        status: Option<String>,
    },
    /// Approve (or reject) one learned `pre_commands` script (§3.2).
    ///
    /// Identified by content digest, not by project: this is arbitrary shell
    /// that will run inside the sandbox of every agent inheriting the profile,
    /// so the thing being approved is the exact script, and editing it asks
    /// again.
    ApprovePreCommands {
        project_id: String,
        digest: String,
        #[arg(long, default_value = "")]
        path: String,
        #[arg(long)]
        reject: bool,
        #[arg(long, default_value = "cli")]
        by: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rc_server=info,tower_http=warn,info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            http_addr,
            grpc_addr,
            allow_anonymous_agents,
            session_ttl_secs,
        } => {
            let cfg = Config {
                data_dir: cli.data_dir,
                http_addr,
                grpc_addr,
                allow_anonymous_agents,
                session_ttl_secs,
            };
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(serve(cfg))
        }
        Command::Admin { username, password, role } => {
            if password.len() < 8 {
                anyhow::bail!("password must be at least 8 characters");
            }
            let store = open_store(cli.data_dir)?;
            store.create_admin(&username, &auth::hash_password(&password)?, &role)?;
            println!("console account `{username}` ({role}) is ready");
            Ok(())
        }
        Command::AgentToken { label } => {
            let store = open_store(cli.data_dir)?;
            let token = rc_core::ids::random_token();
            store.add_agent_token(&auth::hash_token(&token), &label)?;
            println!("{token}");
            eprintln!("(shown once — put it in rc-agent's config as `token`)");
            Ok(())
        }
        Command::EnrollToken { ttl_secs } => {
            let store = open_store(cli.data_dir)?;
            let token = rc_core::ids::random_token();
            store.add_enrollment_token(&token, "cli", ttl_secs)?;
            println!("{token}");
            eprintln!("(single use, expires in {ttl_secs}s)");
            Ok(())
        }
        Command::ApproveEgress { project_id, host, revoke, by } => {
            let host = rc_core::egress::normalize(&host).map_err(|e| anyhow::anyhow!(e))?;
            let store = open_store(cli.data_dir)?;
            let status = if revoke { "rejected" } else { "approved" };
            let changed = store.set_egress_status(&project_id, &host, status, &by)?;
            if changed == 0 {
                anyhow::bail!("no egress request for {project_id} / {host}");
            }
            store.audit(&by, &format!("egress_{status}"), &project_id, &host)?;
            println!("{host} is now {status} for {project_id}");
            Ok(())
        }
        Command::ListPreCommands { status } => {
            let store = open_store(cli.data_dir)?;
            let rows = store.list_pre_commands(status.as_deref())?;
            if rows.is_empty() {
                println!("nothing to show");
                return Ok(());
            }
            for r in rows {
                println!(
                    "\n{} [{}]  project={} path={:?}  by={}",
                    r.digest, r.status, r.project_id, r.path, r.requested_by
                );
                // The script itself, not a summary: approving a digest whose
                // contents nobody printed would be approving nothing.
                for c in &r.commands {
                    println!("    {c}");
                }
            }
            Ok(())
        }
        Command::ApprovePreCommands { project_id, digest, path, reject, by } => {
            let store = open_store(cli.data_dir)?;
            let status = if reject { "rejected" } else { "approved" };
            let changed =
                store.set_pre_commands_status(&project_id, &path, &digest, status, &by)?;
            if changed == 0 {
                anyhow::bail!("no pre_commands request {digest} for {project_id}");
            }
            store.audit(&by, &format!("pre_commands_{status}"), &project_id, &digest)?;
            println!("{digest} is now {status} for {project_id}");
            Ok(())
        }
        Command::ApproveImage { env_id, by } => {
            let store = open_store(cli.data_dir)?;
            store
                .get_image(&env_id)?
                .with_context(|| format!("unknown env {env_id}"))?;
            store.approve_image(&env_id, &by)?;
            store.audit(&by, "approve_image", &env_id, "via cli")?;
            println!("approved {env_id}");
            Ok(())
        }
    }
}

fn open_store(data_dir: PathBuf) -> Result<store::Store> {
    let cfg = Config {
        data_dir,
        http_addr: String::new(),
        grpc_addr: String::new(),
        allow_anonymous_agents: false,
        session_ttl_secs: 3600,
    };
    store::Store::open(&cfg.db_path())
}

async fn serve(cfg: Config) -> Result<()> {
    let http_addr: std::net::SocketAddr = cfg.http_addr.parse().context("parse --http-addr")?;
    let grpc_addr: std::net::SocketAddr = cfg.grpc_addr.parse().context("parse --grpc-addr")?;
    let app = App::new(cfg)?;

    if app.store.admin_count()? == 0 {
        tracing::warn!(
            "no console account exists yet — create one with: rc-server admin --username <name> --password <pw>"
        );
    }
    if app.cfg.allow_anonymous_agents {
        tracing::warn!(
            "--allow-anonymous-agents is on: anything that can reach the gRPC port can submit tasks"
        );
    }

    bg::spawn_all(app.clone());

    let http = {
        let router = admin::router(app.clone())
            .layer(tower_http::compression::CompressionLayer::new())
            .layer(tower_http::trace::TraceLayer::new_for_http());
        let listener = tokio::net::TcpListener::bind(http_addr).await?;
        tracing::info!(%http_addr, "admin console + REST API listening");
        tokio::spawn(async move { axum::serve(listener, router).await })
    };

    let grpc = {
        let app = app.clone();
        tracing::info!(%grpc_addr, "gRPC (agents + workers) listening");
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(grpc_agent::AgentService::new(app.clone()))
                .add_service(grpc_worker::WorkerService::new(app))
                .serve(grpc_addr)
                .await
        })
    };

    tokio::select! {
        r = http => { r??; }
        r = grpc => { r??; }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
        }
    }
    Ok(())
}
