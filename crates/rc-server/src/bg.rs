//! Background loops: dispatch, GC, metric rollup and alerting.

use crate::app::App;
use crate::events::Event;
use crate::images;
use anyhow::Result;
use rc_core::{now_ms, now_secs};
use std::sync::Arc;
use std::time::Duration;

pub fn spawn_all(app: Arc<App>) {
    tokio::spawn(dispatch_loop(app.clone()));
    tokio::spawn(maintenance_loop(app.clone()));
    tokio::spawn(metrics_loop(app.clone()));
    tokio::spawn(alert_loop(app));
}

/// Places queued tasks. Woken by submissions, heartbeats and completions, with
/// a slow tick as a safety net so a lost notification cannot wedge the queue.
async fn dispatch_loop(app: Arc<App>) {
    loop {
        let tick = tokio::time::sleep(Duration::from_secs(2));
        tokio::select! {
            _ = tick => {}
            _ = app.dispatch_signal.notified() => {}
        }
        let n = app.dispatch_once().await;
        if n > 0 {
            tracing::debug!(dispatched = n, "dispatched tasks");
        }
        if let Err(e) = images::dispatch_pending_builds(&app).await {
            tracing::warn!(error = %e, "image build dispatch failed");
        }
    }
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub deleted: u64,
    pub bytes: u64,
}

/// §9. Anything pinned by a live task or still inside its lease window is left
/// alone — the whole point of the lease is that reconciliation promised the
/// agent those blobs would still be there (§4.7).
pub fn collect_garbage(app: &App) -> Result<GcReport> {
    let policy = app.policy();
    let mut report = GcReport::default();
    for blob in app.store.collectable_blobs(policy.blob_gc_ttl_secs, 5000)? {
        let size = app
            .cas
            .size_of(&blob.hash)
            .unwrap_or(blob.size.max(0) as u64);
        if app.cas.remove(&blob.hash).is_ok() {
            app.store.forget_blob(&blob.hash)?;
            report.deleted += 1;
            report.bytes += size;
        }
    }
    if report.deleted > 0 {
        app.metrics.incr("gc_blobs_deleted_total", report.deleted as f64);
        app.metrics.incr("gc_bytes_reclaimed_total", report.bytes as f64);
        tracing::info!(deleted = report.deleted, bytes = report.bytes, "CAS gc");
    }
    Ok(report)
}

async fn maintenance_loop(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    loop {
        ticker.tick().await;
        let policy = app.policy();

        // Abandoned queue entries. Disconnection alone never cancels a task
        // (§5.3, risk #27) — only this TTL does.
        match app.store.expire_pending(policy.pending_ttl_secs) {
            Ok(ids) => {
                for id in ids {
                    app.store.unpin_task_blobs(&id).ok();
                    app.publish_task(&id);
                }
            }
            Err(e) => tracing::error!(error = %e, "pending expiry failed"),
        }

        if let Err(e) = collect_garbage(&app) {
            tracing::error!(error = %e, "gc failed");
        }

        // Workers that stopped heartbeating.
        match app.store.stale_workers(policy.worker_offline_secs) {
            Ok(ids) => {
                for id in ids {
                    if app.workers.is_connected(&id) {
                        continue;
                    }
                    app.store.set_worker_status(&id, "offline").ok();
                    app.store
                        .raise_alert(
                            &format!("worker_offline:{id}"),
                            "warn",
                            &format!("worker {id} has not reported in"),
                        )
                        .ok();
                }
            }
            Err(e) => tracing::error!(error = %e, "worker liveness check failed"),
        }

        // Long-window rollups are pruned on the same schedule (§15.1).
        let now = now_secs();
        app.store.prune_rollups("1min", now - 7 * 24 * 3600).ok();
        app.store.prune_rollups("1hour", now - 90 * 24 * 3600).ok();

        let (blobs, bytes, _) = app.store.cas_summary().unwrap_or((0, 0, 0));
        app.metrics.set("cas_blobs", blobs as f64);
        app.metrics.set("cas_bytes", bytes as f64);
    }
}

async fn metrics_loop(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    loop {
        ticker.tick().await;
        if let Err(e) = app.metrics.flush(&app.store) {
            tracing::error!(error = %e, "metric rollup flush failed");
        }
        if let Ok(counters) = app.store.overview_counters(3600) {
            app.metrics.set("queue_depth", counters.queued as f64);
            app.metrics.set("running_tasks", counters.running as f64);
            app.events.publish(Event::QueueDepth {
                queued: counters.queued,
                running: counters.running,
                at: now_ms(),
            });
        }
        app.metrics
            .set("workers_online", app.workers.online_count() as f64);
        app.metrics
            .set("sse_connections", app.events.subscriber_count() as f64);
    }
}

/// Built-in alert rules (§15.1 layer 3). Deliberately few and obvious;
/// anything richer belongs in Prometheus.
async fn alert_loop(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    loop {
        ticker.tick().await;
        let mut fired = Vec::new();

        if let Ok(c) = app.store.overview_counters(900) {
            if c.queued > 50 {
                fired.push((
                    "queue_backlog".to_string(),
                    "warn",
                    format!("队列积压 {} 个任务", c.queued),
                ));
            } else {
                app.store.resolve_alert("queue_backlog").ok();
            }

            if c.finished_window >= 20 {
                let infra_rate = c.infra_errors_window as f64 / c.finished_window as f64;
                if infra_rate > 0.2 {
                    fired.push((
                        "infra_error_rate".to_string(),
                        "error",
                        format!("15 分钟内 infra_error 占比 {:.0}%", infra_rate * 100.0),
                    ));
                } else {
                    app.store.resolve_alert("infra_error_rate").ok();
                }

                let timeout_rate = c.timeouts_window as f64 / c.finished_window as f64;
                if timeout_rate > 0.2 {
                    fired.push((
                        "timeout_rate".to_string(),
                        "warn",
                        format!("15 分钟内超时占比 {:.0}%", timeout_rate * 100.0),
                    ));
                } else {
                    app.store.resolve_alert("timeout_rate").ok();
                }
            }
        }

        if app.workers.online_count() == 0 {
            fired.push((
                "no_workers".to_string(),
                "error",
                "没有在线 worker，所有任务都会排队".to_string(),
            ));
        } else {
            app.store.resolve_alert("no_workers").ok();
        }

        if let Ok(images) = app.store.list_images(Some("failing")) {
            for img in images {
                fired.push((
                    format!("image_failing:{}", img.id),
                    "warn",
                    format!("镜像 {} 连续 env_error", img.image_ref),
                ));
            }
        }

        for (rule, level, message) in fired {
            match app.store.raise_alert(&rule, level, &message) {
                // Only notify on the transition, never on every tick.
                Ok(true) => {
                    app.events.publish(Event::Alert {
                        rule: rule.clone(),
                        level: level.to_string(),
                        message: message.clone(),
                        at: now_ms(),
                    });
                    notify_webhook(&app, &rule, level, &message).await;
                }
                Ok(false) => {}
                Err(e) => tracing::error!(error = %e, "failed to raise alert"),
            }
        }
    }
}

/// Generic JSON webhook payload that DingTalk, Feishu and Slack all accept.
async fn notify_webhook(app: &App, rule: &str, level: &str, message: &str) {
    let url = app.policy().alert_webhook;
    if url.is_empty() {
        return;
    }
    let text = format!("[remote-compile][{level}] {rule}: {message}");
    let body = serde_json::json!({
        "msgtype": "text",
        "text": { "content": text },
    })
    .to_string();
    // No HTTP client dependency for one call: a task-local minimal POST keeps
    // the dependency surface small, and a failed notification is not fatal.
    if let Err(e) = post_json(&url, &body).await {
        tracing::warn!(error = %e, "alert webhook failed");
    }
}

async fn post_json(url: &str, body: &str) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("only http:// webhooks are supported without a TLS client; put an https proxy in front"))?;
    let (host_port, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host_port.split(':').next().unwrap_or(host_port);
    let port: u16 = host_port
        .split(':')
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(80);

    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await??;
    let req = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn app() -> Arc<App> {
        App::new(Config {
            data_dir: std::env::temp_dir().join(format!("rc-bg-{}", ulid::Ulid::generate())),
            http_addr: "127.0.0.1:0".into(),
            grpc_addr: "127.0.0.1:0".into(),
            allow_anonymous_agents: true,
            session_ttl_secs: 3600,
        })
        .unwrap()
    }

    #[test]
    fn gc_removes_only_cold_unreferenced_blobs() {
        let a = app();
        let cold = a.cas.put(b"cold").unwrap();
        let pinned = a.cas.put(b"pinned").unwrap();
        a.store.touch_blobs(&[(cold.clone(), 4), (pinned.clone(), 6)]).unwrap();
        a.store
            .insert_task(&crate::store::TaskRow { id: "t1".into(), ..Default::default() }, "{}", "{}", "")
            .unwrap();
        a.store.pin_task_blobs("t1", std::slice::from_ref(&pinned)).unwrap();

        let mut p = a.policy();
        p.blob_gc_ttl_secs = -1; // everything is "cold"
        a.set_policy(p).unwrap();

        let report = collect_garbage(&a).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!a.cas.exists(&cold));
        assert!(a.cas.exists(&pinned), "a pinned blob must survive gc (§4.7)");
    }

    #[test]
    fn gc_is_a_no_op_when_everything_is_fresh() {
        let a = app();
        let h = a.cas.put(b"fresh").unwrap();
        a.store.touch_blobs(&[(h.clone(), 5)]).unwrap();
        assert_eq!(collect_garbage(&a).unwrap().deleted, 0);
        assert!(a.cas.exists(&h));
    }

    #[tokio::test]
    async fn a_webhook_without_a_url_is_skipped_silently() {
        let a = app();
        notify_webhook(&a, "rule", "warn", "message").await;
    }

    #[tokio::test]
    async fn https_webhooks_report_a_clear_limitation() {
        let err = post_json("https://example.invalid/hook", "{}").await.unwrap_err();
        assert!(err.to_string().contains("https proxy"));
    }
}
