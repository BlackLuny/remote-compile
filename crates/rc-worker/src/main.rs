//! rc-worker — a Linux compile machine that executes untrusted agent code in
//! a Docker sandbox and reports structured results back to the control plane.

mod client;
mod config;
mod docker;
mod gitmirror;
mod proxy;
mod runner;
mod sysinfo;
mod workspace;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use client::ServerClient;
use config::WorkerConfig;
use docker::Sandbox;
use rc_core::pb;
use runner::Runner;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::StreamExt;

#[derive(Parser)]
#[command(name = "rc-worker", version, about = "remote-compile build worker")]
struct Cli {
    #[arg(long, env = "RC_WORKER_DIR", default_value = "./rc-worker-data", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register with a control plane using a single-use enrollment token.
    Enroll {
        #[arg(long, env = "RC_SERVER", default_value = "http://127.0.0.1:7701")]
        server: String,
        #[arg(long, env = "RC_ENROLLMENT_TOKEN")]
        token: String,
        /// Concurrent builds. Each one gets the configured CPU/memory budget.
        #[arg(long)]
        max_parallel: Option<u32>,
    },
    /// Run the worker.
    Run {
        /// Skip the egress proxy entirely; builds get no network at all.
        #[arg(long)]
        offline: bool,
    },
    /// Stop accepting work, finish what is running, then exit (§8.1).
    Drain,
    /// Remove every container, volume and directory this worker created.
    Uninstall {
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rc_worker=info,info".into()),
        )
        .init();

    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match cli.command {
        Command::Enroll { server, token, max_parallel } => rt.block_on(enroll(cli.data_dir, server, token, max_parallel)),
        Command::Run { offline } => rt.block_on(run(cli.data_dir, offline)),
        Command::Drain => {
            // The running process owns the channel; signalling it through the
            // control plane keeps one code path for drain.
            println!("ask the control plane to drain this worker (Workers page, or the admin API)");
            Ok(())
        }
        Command::Uninstall { yes } => rt.block_on(uninstall(cli.data_dir, yes)),
    }
}

async fn enroll(data_dir: PathBuf, server: String, token: String, max_parallel: Option<u32>) -> Result<()> {
    let mut cfg = WorkerConfig {
        server: server.clone(),
        data_dir: data_dir.clone(),
        ..Default::default()
    };
    if let Some(n) = max_parallel {
        cfg.max_parallel = n.max(1);
    }
    cfg.ensure_dirs()?;

    let disk_gb = sysinfo::disk_free_gb(&cfg.data_dir);
    let resp = ServerClient::enroll(
        &server,
        pb::EnrollReq {
            enrollment_token: token,
            worker_id: String::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arch: cfg.arch(),
            cpu: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
            mem_gb: 0,
            disk_gb,
            max_parallel: cfg.max_parallel,
            labels: cfg.labels.clone().into_iter().collect(),
        },
    )
    .await
    .context("enroll with the control plane")?;

    cfg.worker_id = resp.worker_id.clone();
    cfg.worker_token = resp.worker_token;
    cfg.save()?;
    println!("enrolled as {}", resp.worker_id);
    println!("config written to {}", WorkerConfig::path_in(&data_dir).display());
    println!("now run: rc-worker --data-dir {} run", data_dir.display());
    Ok(())
}

async fn run(data_dir: PathBuf, offline: bool) -> Result<()> {
    let cfg = WorkerConfig::load(&data_dir)?;
    cfg.ensure_dirs()?;

    let sandbox = Arc::new(Sandbox::connect()?);
    let version = sandbox.ping().await.context("ping the docker daemon")?;
    tracing::info!(docker = %version, worker = %cfg.worker_id, "worker starting");

    // §7.1: containers reach nothing but this proxy, and only for allowlisted
    // hosts.
    let proxy_url = if offline {
        tracing::info!("running offline: build containers get --network=none");
        None
    } else {
        match start_proxy(&sandbox, &cfg).await {
            Ok(url) => {
                tracing::info!(%url, "egress proxy listening");
                Some(url)
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not start the egress proxy; builds will run offline");
                None
            }
        }
    };

    let runner = Arc::new(Runner::new(cfg.clone(), sandbox.clone(), proxy_url)?);

    // Crash recovery (§8.1): anything of ours still running belongs to a task
    // that no longer exists.
    match sandbox.reconcile(&[]).await {
        Ok(n) if n > 0 => tracing::info!(removed = n, "cleaned up containers from a previous run"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "reconcile failed"),
    }

    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        match session(&cfg, runner.clone()).await {
            Ok(()) => backoff = std::time::Duration::from_secs(1),
            Err(e) => {
                // A rejected token will be rejected again forever; retrying it
                // just fills the log and hides the real problem.
                if let Some(status) = e.downcast_ref::<tonic::Status>() {
                    if !client::is_transient(status) {
                        anyhow::bail!("control plane refused this worker: {status}");
                    }
                }
                tracing::error!(error = %e, retry_in = ?backoff, "control-plane session ended");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
    }
}

async fn start_proxy(sandbox: &Sandbox, cfg: &WorkerConfig) -> Result<String> {
    let gateway = sandbox.ensure_egress_network().await?;
    let server = Arc::new(proxy::ProxyServer {
        allowlist: proxy::Allowlist::new(cfg.allowlist.clone()),
        byte_cap: cfg.egress_byte_cap,
    });
    // Bind on the bridge gateway so only containers on rc-egress can reach it.
    let addr = format!("{gateway}:0")
        .parse()
        .context("parse the egress gateway address")?;
    let bound = server.bind(addr).await?;
    Ok(format!("http://{gateway}:{}", bound.port()))
}

/// One connection to the control plane, from opening the channel to losing it.
async fn session(cfg: &WorkerConfig, runner: Arc<Runner>) -> Result<()> {
    let mut client = ServerClient::connect(&cfg.server, &cfg.worker_id, &cfg.worker_token).await?;
    let (events_tx, events_rx) = mpsc::channel::<pb::WorkerEvent>(256);
    let mut commands = client.open_channel(events_rx).await?;
    tracing::info!(server = %cfg.server, "channel open");

    let active: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let heartbeat = {
        let cfg = cfg.clone();
        let runner = runner.clone();
        let tx = events_tx.clone();
        let active = active.clone();
        let draining = draining.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                ticker.tick().await;
                let running: Vec<String> = active.lock().await.iter().cloned().collect();
                let stats = collect_stats(&cfg, &runner, running.len() as u32).await;
                let status = if draining.load(std::sync::atomic::Ordering::Relaxed) {
                    "draining"
                } else {
                    "online"
                };
                if tx
                    .send(client::heartbeat_event(stats, status, running))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    };

    // Idle caches are reclaimed on a slow timer rather than on the hot path.
    let gc = {
        let runner = runner.clone();
        let idle_days = cfg.worktree_idle_days;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                ticker.tick().await;
                match runner.gc(idle_days).await {
                    Ok(n) if n > 0 => tracing::info!(reclaimed = n, "worktree caches reclaimed"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "worker gc failed"),
                }
            }
        })
    };

    let permits = Arc::new(tokio::sync::Semaphore::new(cfg.max_parallel.max(1) as usize));

    while let Some(cmd) = commands.next().await {
        let cmd = match cmd {
            Ok(c) => c,
            Err(status) => {
                heartbeat.abort();
                gc.abort();
                return Err(anyhow::Error::new(status));
            }
        };
        let Some(body) = cmd.body else { continue };
        match body {
            pb::server_cmd::Body::Assign(assignment) => {
                if draining.load(std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(task = %assignment.task_id, "refusing work while draining");
                    let _ = events_tx
                        .send(client::done_event(client::infra_failure(
                            &assignment.task_id,
                            "worker is draining",
                        )))
                        .await;
                    continue;
                }
                let permits = permits.clone();
                let runner = runner.clone();
                let tx = events_tx.clone();
                let active = active.clone();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _permit = permits.acquire().await;
                    let task_id = assignment.task_id.clone();
                    active.lock().await.insert(task_id.clone());

                    // Each task gets its own connection for blob transfer so a
                    // slow download cannot stall the command channel.
                    let done = match ServerClient::connect(&cfg.server, &cfg.worker_id, &cfg.worker_token).await {
                        Ok(mut blob_client) => runner.execute(assignment, &mut blob_client, tx.clone()).await,
                        Err(e) => client::infra_failure(&task_id, e),
                    };
                    active.lock().await.remove(&task_id);
                    let _ = tx.send(client::done_event(done)).await;
                });
            }
            pb::server_cmd::Body::CancelTaskId(task_id) => {
                runner.cancel(&task_id).await;
            }
            pb::server_cmd::Body::Drain(_) => {
                tracing::info!("draining: no new tasks will be accepted");
                draining.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            pb::server_cmd::Body::BuildImage(order) => {
                let runner = runner.clone();
                let tx = events_tx.clone();
                tokio::spawn(async move {
                    if let Some(done) = runner.build_image(order).await {
                        let _ = tx.send(client::image_done_event(done)).await;
                    }
                });
            }
            pb::server_cmd::Body::Ping(_) => {}
        }
    }

    heartbeat.abort();
    gc.abort();
    Ok(())
}

async fn collect_stats(cfg: &WorkerConfig, runner: &Runner, running: u32) -> pb::WorkerStats {
    let cached_worktrees = list_dir_names(&cfg.work_dir());
    let cached_projects = list_dir_names(&cfg.mirror_dir())
        .into_iter()
        .map(|n| n.trim_end_matches(".git").to_string())
        .collect();
    let cached_images = runner
        .sandbox
        .our_volumes()
        .await
        .map(|v| v.into_iter().map(|(n, _)| n).collect())
        .unwrap_or_default();
    pb::WorkerStats {
        cpu_load: sysinfo::cpu_load(),
        disk_free_gb: sysinfo::disk_free_gb(&cfg.data_dir),
        running_tasks: running,
        cached_worktrees,
        cached_projects,
        cached_images,
        sccache_hit_rate: 0.0,
        gc_runs: 0,
        gc_reclaimed_mb: 0,
    }
}

fn list_dir_names(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

async fn uninstall(data_dir: PathBuf, yes: bool) -> Result<()> {
    if !yes {
        println!(
            "this removes every rc.* container and volume on this host plus {}\nre-run with --yes to proceed",
            data_dir.display()
        );
        return Ok(());
    }
    if let Ok(sandbox) = Sandbox::connect() {
        match sandbox.reconcile(&[]).await {
            Ok(n) => println!("removed {n} container(s)"),
            Err(e) => eprintln!("container cleanup failed: {e}"),
        }
        match sandbox.our_volumes().await {
            Ok(volumes) => {
                for (name, _) in volumes {
                    match sandbox.remove_volume(&name).await {
                        Ok(()) => println!("removed volume {name}"),
                        Err(e) => eprintln!("volume {name}: {e}"),
                    }
                }
            }
            Err(e) => eprintln!("volume listing failed: {e}"),
        }
    } else {
        eprintln!("docker is unreachable; skipping container and volume cleanup");
    }
    if data_dir.exists() {
        std::fs::remove_dir_all(&data_dir)
            .with_context(|| format!("remove {}", data_dir.display()))?;
        println!("removed {}", data_dir.display());
    }
    Ok(())
}
