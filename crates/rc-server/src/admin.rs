//! Admin REST API, SSE stream and embedded console hosting (§14).
//!
//! The console talks only to this JSON surface — never gRPC-web — so the
//! frontend build stays a plain SPA and the whole thing ships as one binary.

use crate::app::{App, CleanupOutcome};
use crate::auth::{self, SESSION_COOKIE};
use crate::config::Policy;
use crate::events::Event;
use crate::images;
use crate::store::TaskFilter;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use rc_core::model::Role;
use rc_core::{ids, now_secs, pb};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

// ------------------------------- errors -------------------------------

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::NOT_FOUND, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------- authentication ----------------------------

/// Any authenticated console user (§14.2 `viewer` or `admin`).
#[derive(Clone, Debug)]
pub struct User {
    pub username: String,
    pub role: Role,
}

/// A user allowed to change things. Every mutating route takes this instead of
/// [`User`], so read-only access cannot be widened by forgetting a check.
#[derive(Clone, Debug)]
pub struct AdminUser(pub User);

fn user_from_headers(app: &App, headers: &HeaderMap) -> Option<User> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    let token = auth::cookie_value(cookie, SESSION_COOKIE)?;
    let (username, role) = app.store.lookup_session(&token).ok()??;
    Some(User {
        username,
        role: Role::parse_or_default(&role),
    })
}

impl axum::extract::FromRequestParts<Arc<App>> for User {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        user_from_headers(app, &parts.headers)
            .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "not signed in".into()))
    }
}

impl axum::extract::FromRequestParts<Arc<App>> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        let user = User::from_request_parts(parts, app).await?;
        if !user.role.can_write() {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "this action requires the admin role".into(),
            ));
        }
        Ok(AdminUser(user))
    }
}

// ------------------------------- router -------------------------------

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/bootstrap", get(bootstrap_state))
        .route("/api/overview", get(overview))
        .route("/api/events", get(sse_events))
        .route("/api/series", get(series))
        .route("/api/workers", get(list_workers))
        .route("/api/workers/{id}", get(get_worker).delete(delete_worker))
        .route("/api/workers/{id}/drain", post(drain_worker))
        .route("/api/workers/{id}/resume", post(resume_worker))
        .route("/api/workers/{id}/rename", post(rename_worker))
        .route(
            "/api/workers/{id}/cleanup",
            get(cleanup_worker_status).post(cleanup_worker),
        )
        .route(
            "/api/workers/{id}/cleanup/{request_id}",
            get(get_cleanup_worker_job),
        )
        .route("/api/enrollment-tokens", get(list_enrollment).post(create_enrollment))
        .route("/api/agent-tokens", get(list_agent_tokens).post(create_agent_token))
        .route("/api/agent-tokens/{hash}", delete(delete_agent_token))
        .route("/api/tasks", get(list_tasks))
        .route("/api/tasks/{id}", get(get_task))
        .route("/api/tasks/{id}/log", get(get_task_log))
        .route("/api/tasks/{id}/cancel", post(cancel_task))
        .route("/api/images", get(list_images))
        .route("/api/images/{id}", get(get_image))
        .route("/api/images/{id}/approve", post(approve_image))
        .route("/api/images/{id}/reject", post(reject_image))
        .route("/api/images/{id}/rebuild", post(rebuild_image))
        .route("/api/images/{id}/push", post(push_image))
        .route("/api/images/{id}/pull", post(pull_image))
        .route("/api/egress", get(list_egress))
        .route("/api/egress/decide", post(decide_egress))
        .route("/api/pre-commands", get(list_pre_commands))
        .route("/api/pre-commands/decide", post(decide_pre_commands))
        .route("/api/profiles", get(list_profiles))
        .route("/api/profiles/{id}", put(update_profile).delete(delete_profile))
        .route("/api/projects", get(list_projects))
        .route("/api/storage", get(storage))
        .route("/api/storage/gc", post(run_gc))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/admins", get(list_admins).post(create_admin))
        .route("/api/admins/{username}", delete(delete_admin))
        .route("/api/audit", get(audit))
        .route("/api/alerts", get(alerts))
        .route("/metrics", get(prometheus))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(get(crate::assets::serve))
        .with_state(app)
}

// -------------------------------- auth --------------------------------

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(
    State(app): State<Arc<App>>,
    Json(req): Json<LoginReq>,
) -> ApiResult<Response> {
    let Some((hash, role)) = app.store.get_admin(&req.username).map_err(ApiError::from)? else {
        // Same message and roughly the same cost for both failure modes, so
        // the endpoint does not enumerate usernames.
        return Err(ApiError(StatusCode::UNAUTHORIZED, "用户名或密码错误".into()));
    };
    if !auth::verify_password(&hash, &req.password) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "用户名或密码错误".into()));
    }
    let token = ids::random_token();
    app.store
        .create_session(&token, &req.username, &role, app.cfg.session_ttl_secs)
        .map_err(ApiError::from)?;
    app.store.audit(&req.username, "login", "", "").ok();

    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        app.cfg.session_ttl_secs
    );
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "username": req.username, "role": role })),
    )
        .into_response())
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(cookie) = headers.get(header::COOKIE).and_then(|c| c.to_str().ok()) {
        if let Some(token) = auth::cookie_value(cookie, SESSION_COOKIE) {
            app.store.delete_session(&token).ok();
        }
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!("{SESSION_COOKIE}=; Path=/; HttpOnly; Max-Age=0"),
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn me(user: User) -> Json<serde_json::Value> {
    Json(json!({ "username": user.username, "role": user.role.as_str() }))
}

/// Lets the login screen tell "no admin exists yet" from "please sign in".
async fn bootstrap_state(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "needs_setup": app.store.admin_count().map_err(ApiError::from)? == 0,
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

// ------------------------------ overview ------------------------------

#[derive(Deserialize)]
struct WindowQuery {
    #[serde(default)]
    window_secs: Option<i64>,
}

async fn overview(
    _u: User,
    State(app): State<Arc<App>>,
    Query(q): Query<WindowQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let window = q.window_secs.unwrap_or(24 * 3600);
    let counters = app.store.overview_counters(window).map_err(ApiError::from)?;
    let workers: Vec<_> = app
        .workers
        .snapshot()
        .into_iter()
        .map(worker_json)
        .collect();
    let percentiles = app.store.phase_percentiles(window).map_err(ApiError::from)?;
    let histogram = app.store.task_histogram(300, 48).map_err(ApiError::from)?;
    let (cas_count, cas_bytes, cas_pinned) = app.store.cas_summary().map_err(ApiError::from)?;
    let recent = app
        .store
        .list_tasks(&TaskFilter { limit: Some(15), ..Default::default() })
        .map_err(ApiError::from)?;
    let names = app.store.worker_names().map_err(ApiError::from)?;

    let (metric_counters, metric_gauges) = app.metrics.snapshot();

    let success_rate = if counters.finished_window > 0 {
        counters.success_window as f64 / counters.finished_window as f64
    } else {
        0.0
    };
    let cache_hit_rate = if counters.finished_window > 0 {
        counters.cache_hits_window as f64 / counters.finished_window as f64
    } else {
        0.0
    };

    Ok(Json(json!({
        "counters": counters,
        "success_rate": success_rate,
        "cache_hit_rate": cache_hit_rate,
        "workers_online": app.workers.online_count(),
        "workers": workers,
        "phase_percentiles": percentiles.iter().map(|(n, p50, p95)| json!({
            "phase": n, "p50": p50, "p95": p95
        })).collect::<Vec<_>>(),
        "histogram": histogram.iter().map(|(ts, total, success, cached)| json!({
            "ts": ts, "total": total, "success": success, "cache_hit": cached
        })).collect::<Vec<_>>(),
        "storage": { "blobs": cas_count, "bytes": cas_bytes, "pinned": cas_pinned },
        "alerts": app.store.list_alerts(false).map_err(ApiError::from)?,
        "recent_tasks": recent.iter().map(|t| task_json(t, &names)).collect::<Vec<_>>(),
        "metrics": { "counters": metric_counters, "gauges": metric_gauges },
    })))
}

/// Longest worker display name the console will accept. Names sit in table
/// cells next to the id and status, so a paragraph is never useful there.
const WORKER_NAME_MAX: usize = 48;

fn worker_json(w: crate::workers::WorkerConn) -> serde_json::Value {
    json!({
        "id": w.id,
        "name": w.name,
        "arch": w.arch,
        "version": w.version,
        "status": w.status,
        "max_parallel": w.max_parallel,
        "free_slots": w.free_slots(),
        "last_hb_ms": w.last_hb_ms,
        "connected_at": w.connected_at,
        "stats": {
            "cpu_load": w.stats.cpu_load,
            "disk_free_gb": w.stats.disk_free_gb,
            // 0 means the worker does not report a size; the console says
            // "unknown" rather than drawing a 0% bar.
            "disk_total_gb": w.stats.disk_total_gb,
            "container_limits": w.stats.container_limits,
            "running_tasks": w.stats.running_tasks,
            "cached_worktrees": w.stats.cached_worktrees,
            "cached_projects": w.stats.cached_projects,
            "cached_images": w.stats.cached_images,
            "sccache_hit_rate": w.stats.sccache_hit_rate,
            "gc_runs": w.stats.gc_runs,
            "gc_reclaimed_mb": w.stats.gc_reclaimed_mb,
        },
    })
}

/// The operator-given name of a worker, or `""` when it has none.
fn worker_name<'a>(names: &'a std::collections::HashMap<String, String>, id: &str) -> &'a str {
    names.get(id).map(String::as_str).unwrap_or("")
}

/// Task rows keep `worker_id` because that is the identity (it keys tokens,
/// history and scheduler affinity). The console renders the machine an operator
/// renamed, so the display name rides along as an extra field — it is never
/// written back into task history.
fn task_json(
    task: &crate::store::TaskRow,
    names: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let mut v = json!(task);
    v["worker_name"] = json!(worker_name(names, &task.worker_id));
    v
}

/// Live updates (§14.1). SSE rather than WebSocket: the traffic is one-way and
/// browsers reconnect on their own.
async fn sse_events(
    _u: User,
    State(app): State<Arc<App>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    app.metrics
        .set("sse_connections", (app.events.subscriber_count() + 1) as f64);
    let stream = BroadcastStream::new(app.events.subscribe()).filter_map(|item| match item {
        Ok(ev) => Some(Ok(SseEvent::default()
            .event("update")
            .data(serde_json::to_string(&ev).unwrap_or_default()))),
        // A lagging console just misses a few frames; it re-reads on the next
        // poll rather than tearing down the stream.
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct SeriesQuery {
    metric: String,
    #[serde(default)]
    granularity: Option<String>,
    #[serde(default)]
    since: Option<i64>,
}

async fn series(
    _u: User,
    State(app): State<Arc<App>>,
    Query(q): Query<SeriesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let granularity = q.granularity.unwrap_or_else(|| "1min".into());
    let since = q.since.unwrap_or_else(|| now_secs() - 6 * 3600);
    let points = app
        .store
        .read_series(&q.metric, &granularity, since)
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "metric": q.metric,
        "granularity": granularity,
        "points": points.iter().map(|(ts, sum, count)| json!({
            "ts": ts, "sum": sum, "count": count,
            "avg": if *count > 0 { sum / *count as f64 } else { 0.0 }
        })).collect::<Vec<_>>(),
    })))
}

// ------------------------------- workers -------------------------------

async fn list_workers(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let live: std::collections::HashMap<String, serde_json::Value> = app
        .workers
        .snapshot()
        .into_iter()
        .map(|w| (w.id.clone(), worker_json(w)))
        .collect();
    let rows = app.store.list_workers().map_err(ApiError::from)?;
    let merged: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            let mut base = json!({
                "id": r.id, "name": r.name, "arch": r.arch, "labels": r.labels,
                "capacity": r.capacity, "status": r.status, "version": r.version,
                "max_parallel": r.max_parallel, "enrolled_at": r.enrolled_at,
                "last_hb": r.last_hb, "connected": false,
            });
            if let Some(l) = live.get(&r.id) {
                base["connected"] = json!(true);
                base["status"] = l["status"].clone();
                base["stats"] = l["stats"].clone();
                base["free_slots"] = l["free_slots"].clone();
                // `version` and `arch` are enrollment facts, so a worker
                // upgraded in place keeps reporting whatever it first arrived
                // with — which is how a fleet-wide upgrade used to look like
                // nothing happened (§8.1). The handshake knows the truth; use
                // it whenever the worker is connected.
                base["version"] = l["version"].clone();
                base["arch"] = l["arch"].clone();
            }
            base
        })
        .collect();
    Ok(Json(json!({ "workers": merged })))
}

async fn get_worker(
    _u: User,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = app
        .store
        .list_workers()
        .map_err(ApiError::from)?
        .into_iter()
        .find(|w| w.id == id)
        .ok_or_else(|| ApiError::not_found("unknown worker"))?;
    let live = app.workers.get(&id).map(worker_json);
    let tasks = app.store.tasks_on_worker(&id).map_err(ApiError::from)?;
    Ok(Json(json!({ "worker": row, "live": live, "running_tasks": tasks })))
}

/// Drain lets a worker finish what it has and stop taking new work (§8.1).
async fn drain_worker(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Pin survives heartbeats until resume; worker also flips its local flag.
    app.workers.set_status(&id, "draining");
    app.store.set_worker_status(&id, "draining").map_err(ApiError::from)?;
    app.workers
        .send(&id, pb::ServerCmd { body: Some(pb::server_cmd::Body::Drain(pb::Empty {})) })
        .await;
    app.store.audit(&u.username, "drain_worker", &id, "").ok();
    Ok(Json(json!({ "ok": true })))
}

async fn resume_worker(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Clears admin_drain pin so subsequent heartbeats can report online.
    // Note: the worker process still has a local drain flag with no Resume
    // wire command; reconnect/restart is needed for a fully drained worker.
    app.workers.set_status(&id, "online");
    app.store.set_worker_status(&id, "online").map_err(ApiError::from)?;
    app.store.audit(&u.username, "resume_worker", &id, "").ok();
    app.dispatch_signal.notify_one();
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct RenameReq {
    #[serde(default)]
    name: String,
}

/// Give a worker a human name for the console.
///
/// Only the display name changes. `id` is the enrollment identity: it keys the
/// token hash, the task history and the scheduler's cache affinity, so a rename
/// that touched it would orphan running work and strand cached volumes.
/// An empty `name` clears the alias and falls back to the id.
async fn rename_worker(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(req): Json<RenameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = req.name.trim();
    if name.chars().count() > WORKER_NAME_MAX {
        return Err(ApiError::bad(format!(
            "name must be at most {WORKER_NAME_MAX} characters"
        )));
    }
    // A name is rendered next to status and task counts in tight table cells;
    // control characters would either break the layout or hide the id it is
    // meant to clarify.
    if name.chars().any(|c| c.is_control()) {
        return Err(ApiError::bad("name must not contain control characters"));
    }
    if !app.store.set_worker_name(&id, name).map_err(ApiError::from)? {
        return Err(ApiError::not_found("unknown worker"));
    }
    // The overview reads the live registry, so update both or the console
    // shows two different names for the same machine.
    app.workers.set_name(&id, name);
    app.store.audit(&u.username, "rename_worker", &id, name).ok();
    Ok(Json(json!({ "ok": true, "name": name })))
}

#[derive(Deserialize)]
struct CleanupReq {
    /// When true, reclaim every worktree cache not currently running a task.
    #[serde(default)]
    all_unused: bool,
    /// Used when `all_unused` is false. Default 14 matches worker idle GC.
    #[serde(default = "default_cleanup_idle_days")]
    idle_days: i64,
}

fn default_cleanup_idle_days() -> i64 {
    14
}

/// How long the control plane waits for a worker to finish reclaiming volumes.
const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Ensures the per-worker cleanup lock and draining pin are always released if
/// the request or background watcher is cancelled or dropped before finishing.
struct CleanupGuard {
    app: Arc<App>,
    worker_id: String,
    request_id: String,
    conn_gen: u64,
    prior_status: String,
    prior_was_draining: bool,
    armed: bool,
}

impl CleanupGuard {
    fn settle(&mut self, outcome: CleanupOutcome) -> bool {
        if !self.armed {
            return false;
        }
        self.armed = false;
        self.app.settle_cleanup_and_restore(
            &self.worker_id,
            &self.request_id,
            self.conn_gen,
            outcome,
            &self.prior_status,
            self.prior_was_draining,
        )
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        self.app.settle_cleanup_and_restore(
            &self.worker_id,
            &self.request_id,
            self.conn_gen,
            CleanupOutcome::Failed("cleanup aborted before completion".into()),
            &self.prior_status,
            self.prior_was_draining,
        );
    }
}

/// Start manual disk reclaim on one worker asynchronously. Temporarily drains
/// it so new work cannot race the delete, leaves in-flight tasks alone, and
/// returns the job handle immediately so callers can poll for completion without
/// holding a 300s HTTP connection open.
async fn cleanup_worker(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<CleanupReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    if !body.all_unused && body.idle_days < 0 {
        return Err(ApiError::bad("idle_days must be >= 0"));
    }
    let Some(w) = app.workers.get(&id) else {
        return Err(ApiError::bad(format!("worker {id} is not connected")));
    };

    let idle_days = body.idle_days.max(0);
    let conn_gen = w.conn_gen;
    let prior_status = if w.admin_drain {
        "draining".to_string()
    } else {
        w.status
    };
    let prior_was_draining = prior_status == "draining";
    let request_id = ids::random_token();

    let Some(rx) = app.try_begin_cleanup(&id, &request_id, conn_gen, body.all_unused, idle_days) else {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("worker {id} already has a cleanup in progress"),
        ));
    };

    let mut guard = CleanupGuard {
        app: app.clone(),
        worker_id: id.clone(),
        request_id: request_id.clone(),
        conn_gen,
        prior_status: prior_status.clone(),
        prior_was_draining,
        armed: true,
    };

    let order = pb::CleanupOrder {
        request_id: request_id.clone(),
        all_unused: body.all_unused,
        idle_days,
    };

    // Keep the scheduler off this machine for the duration of the pass.
    // admin_drain pin blocks "online" heartbeats until we clear it.
    if !app.workers.set_status_if_gen(&id, conn_gen, "draining") {
        guard.settle(CleanupOutcome::Failed("worker is not connected".into()));
        return Err(ApiError::bad(format!("worker {id} is not connected")));
    }
    if app.workers.get(&id).is_some_and(|w| w.conn_gen == conn_gen) {
        app.store.set_worker_status(&id, "draining")?;
    }

    let sent = app
        .workers
        .send_if_gen(
            &id,
            conn_gen,
            pb::ServerCmd {
                body: Some(pb::server_cmd::Body::Cleanup(order)),
            },
        )
        .await;
    if !sent {
        guard.settle(CleanupOutcome::Failed("worker is not connected".into()));
        return Err(ApiError::bad(format!("worker {id} is not connected")));
    }

    let initial = app.get_cleanup_job(&id, &request_id);
    let actor = u.username;
    let all_unused = body.all_unused;

    tokio::spawn(watch_cleanup(
        rx,
        guard,
        actor,
        all_unused,
        idle_days,
        CLEANUP_TIMEOUT,
    ));

    Ok((StatusCode::ACCEPTED, Json(json!(initial))))
}

async fn watch_cleanup(
    rx: tokio::sync::oneshot::Receiver<pb::CleanupDone>,
    mut guard: CleanupGuard,
    actor: String,
    all_unused: bool,
    idle_days: i64,
    timeout: std::time::Duration,
) {
    let app = guard.app.clone();
    let id = guard.worker_id.clone();
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(done)) => {
            let reclaimed = done.reclaimed;
            let ok = done.ok;
            guard.settle(CleanupOutcome::Done(done));
            app.store
                .audit(
                    &actor,
                    "cleanup_worker",
                    &id,
                    &format!(
                        "all_unused={} idle_days={} reclaimed={} ok={}",
                        all_unused, idle_days, reclaimed, ok
                    ),
                )
                .ok();
        }
        Ok(Err(_)) => {
            guard.settle(CleanupOutcome::Failed(
                "cleanup waiter dropped (worker disconnected?)".into(),
            ));
            app.store
                .audit(
                    &actor,
                    "cleanup_worker",
                    &id,
                    &format!(
                        "all_unused={} idle_days={} reclaimed=0 ok=false (disconnected)",
                        all_unused, idle_days
                    ),
                )
                .ok();
        }
        Err(_) => {
            // Leave admin_drain pinned only if the machine was already drained;
            // otherwise free the pin so a late heartbeat can report online once
            // the worker finishes and clears its local flag.
            guard.settle(CleanupOutcome::Failed(format!(
                "cleanup timed out after {}s",
                timeout.as_secs()
            )));
            app.store
                .audit(
                    &actor,
                    "cleanup_worker",
                    &id,
                    &format!(
                        "all_unused={} idle_days={} reclaimed=0 ok=false (timeout)",
                        all_unused, idle_days
                    ),
                )
                .ok();
        }
    }
}

async fn cleanup_worker_status(
    _u: User,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let (active, latest) = app.worker_cleanup_summary(&id);
    Ok(Json(json!({ "active": active, "latest": latest })))
}

async fn get_cleanup_worker_job(
    _u: User,
    State(app): State<Arc<App>>,
    Path((id, request_id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let job = app
        .get_cleanup_job(&id, &request_id)
        .ok_or_else(|| ApiError::not_found("unknown cleanup job"))?;
    Ok(Json(json!(job)))
}

async fn delete_worker(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    app.workers.disconnect(&id);
    app.clear_worker_cleanups(&id);
    app.reclaim_worker_tasks(&id).map_err(ApiError::from)?;
    app.store.delete_worker(&id).map_err(ApiError::from)?;
    app.store.audit(&u.username, "delete_worker", &id, "").ok();
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct EnrollmentReq {
    #[serde(default)]
    ttl_secs: Option<i64>,
}

async fn create_enrollment(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Json(req): Json<EnrollmentReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = ids::random_token();
    let ttl = req.ttl_secs.unwrap_or(3600);
    app.store
        .add_enrollment_token(&token, &u.username, ttl)
        .map_err(ApiError::from)?;
    app.store.audit(&u.username, "create_enrollment_token", "", "").ok();
    // Shown once; only its consumption is recorded afterwards.
    Ok(Json(json!({ "token": token, "expires_in": ttl })))
}

async fn list_enrollment(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let rows = app.store.list_enrollment_tokens().map_err(ApiError::from)?;
    Ok(Json(json!({
        "tokens": rows.iter().map(|t| json!({
            // Never echo a live token back; a truncated fingerprint is enough
            // to correlate it with the one shown at creation time.
            "fingerprint": &t.token[..8.min(t.token.len())],
            "created_by": t.created_by, "created_at": t.created_at,
            "expires_at": t.expires_at, "used_at": t.used_at, "used_by": t.used_by,
        })).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct AgentTokenReq {
    label: String,
}

async fn create_agent_token(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Json(req): Json<AgentTokenReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = ids::random_token();
    app.store
        .add_agent_token(&auth::hash_token(&token), &req.label)
        .map_err(ApiError::from)?;
    app.store.audit(&u.username, "create_agent_token", &req.label, "").ok();
    Ok(Json(json!({ "token": token, "label": req.label })))
}

async fn list_agent_tokens(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let rows = app.store.list_agent_tokens().map_err(ApiError::from)?;
    Ok(Json(json!({
        "tokens": rows.iter().map(|(hash, label, at, used)| json!({
            "hash": hash, "label": label, "created_at": at, "last_used": used
        })).collect::<Vec<_>>()
    })))
}

async fn delete_agent_token(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(hash): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    app.store.delete_agent_token(&hash).map_err(ApiError::from)?;
    app.store.audit(&u.username, "delete_agent_token", &hash, "").ok();
    Ok(Json(json!({ "ok": true })))
}

// -------------------------------- tasks --------------------------------

async fn list_tasks(
    _u: User,
    State(app): State<Arc<App>>,
    Query(f): Query<TaskFilter>,
) -> ApiResult<Json<serde_json::Value>> {
    let tasks = app.store.list_tasks(&f).map_err(ApiError::from)?;
    let names = app.store.worker_names().map_err(ApiError::from)?;
    let total = app.store.count_tasks(&f).map_err(ApiError::from)?;
    Ok(Json(json!({
        "tasks": tasks.iter().map(|t| task_json(t, &names)).collect::<Vec<_>>(),
        "total": total
    })))
}

async fn get_task(
    _u: User,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let task = app
        .store
        .get_task(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("unknown task"))?;
    let timeline = app.store.timeline(&id).map_err(ApiError::from)?;
    let attempts = app.store.attempt_records(&id).map_err(ApiError::from)?;
    let inputs = app.store.get_task_inputs(&id).map_err(ApiError::from)?;
    let names = app.store.worker_names().map_err(ApiError::from)?;
    // A task sitting in the queue is the case operators most often need
    // explained, so say why no worker took it.
    let placement = if task.status == "queued" {
        app.explain_placement(&task)
            .into_iter()
            .map(|(w, r)| {
                json!({ "worker_name": worker_name(&names, &w), "worker_id": w, "reason": r })
            })
            .collect::<Vec<_>>()
    } else {
        vec![]
    };
    Ok(Json(json!({
        "task": task_json(&task, &names),
        "placement": placement,
        "result": task.result(),
        "timeline": timeline.iter().map(|p| json!({
            "phase": p.phase, "at_ms": p.at_ms, "worker_id": p.worker_id, "detail": p.detail
        })).collect::<Vec<_>>(),
        "attempts": attempts.iter().map(|(w, at, err)| json!({
            "worker_id": w, "worker_name": worker_name(&names, w), "at": at, "error": err
        })).collect::<Vec<_>>(),
        "profile": inputs.as_ref().map(|(_, p, _)| p.clone()),
        "base_commit": inputs.as_ref().map(|(_, _, b)| b.clone()),
        // Which local directories this task actually synced. Without it an
        // operator seeing a workspace several times the size of the repository
        // has no way to find out why.
        "roots": inputs
            .as_ref()
            .and_then(|(m, _, _)| serde_json::from_str::<Option<pb::Manifest>>(m).ok().flatten())
            .map(|m| json!({
                "anchor_mount": m.anchor_mount,
                "entries": m.entries.len(),
                "roots": m.roots.iter().map(|r| json!({
                    "mount": r.mount,
                    "local_path": r.local_path,
                    "primary": r.primary,
                    "bytes": r.bytes,
                    "files": r.files,
                })).collect::<Vec<_>>(),
            })),
    })))
}

#[derive(Deserialize)]
struct LogParams {
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    limit: u32,
    #[serde(default)]
    grep: String,
    #[serde(default)]
    tail: bool,
}

async fn get_task_log(
    _u: User,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(p): Query<LogParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let chunk = app
        .get_log(&pb::LogQuery {
            task_id: id,
            offset: p.offset,
            limit: p.limit,
            grep: p.grep,
            tail: p.tail,
            ..Default::default()
        })
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "lines": chunk.lines, "offset": chunk.offset,
        "total_lines": chunk.total_lines, "truncated": chunk.truncated
    })))
}

async fn cancel_task(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let task = app
        .store
        .get_task(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("unknown task"))?;
    if rc_core::TaskState::parse_or_default(&task.status).is_terminal() {
        return Err(ApiError::bad("task already finished"));
    }
    app.store.set_status(&id, "canceled").map_err(ApiError::from)?;
    app.store.unpin_task_blobs(&id).map_err(ApiError::from)?;
    // Re-read: the task may have moved to another worker since the snapshot.
    let worker_id = app
        .store
        .get_task(&id)
        .map_err(ApiError::from)?
        .map(|t| t.worker_id)
        .unwrap_or_default();
    if !worker_id.is_empty() {
        app.workers
            .send(
                &worker_id,
                pb::ServerCmd { body: Some(pb::server_cmd::Body::CancelTaskId(id.clone())) },
            )
            .await;
    }
    app.store.audit(&u.username, "cancel_task", &id, "").ok();
    app.publish_task(&id);
    Ok(Json(json!({ "ok": true })))
}

// -------------------------------- images --------------------------------

#[derive(Deserialize)]
struct StatusQuery {
    #[serde(default)]
    status: Option<String>,
}

async fn list_images(
    _u: User,
    State(app): State<Arc<App>>,
    Query(q): Query<StatusQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = app
        .store
        .list_images(q.status.as_deref())
        .map_err(ApiError::from)?;
    let policy = app.policy();
    let enriched: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            let health = images::health_of(&r);
            let mirror = images::mirror_status(&app, &r.id);
            let remote_ref = policy.image_remote_ref(&r.id).unwrap_or_default();
            json!({
                "image": r,
                "full_ref": images::full_ref(&r),
                "remote_ref": remote_ref,
                "mirror": mirror,
                "health": {
                    "last_success_at": health.last_success_at,
                    "success_rate_7d": health.success_rate_7d,
                    "total_runs": health.total_runs,
                },
            })
        })
        .collect();
    Ok(Json(json!({
        "images": enriched,
        "registry": {
            "enabled": policy.image_registry_enabled,
            "host": policy.image_registry,
            "prefix": policy.image_registry_prefix,
        },
    })))
}

async fn get_image(
    _u: User,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = app
        .store
        .get_image(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("unknown image"))?;
    let used_by = app.store.image_usage(&row.digest).map_err(ApiError::from)?;
    Ok(Json(json!({
        "image": row, "full_ref": images::full_ref(&row), "used_by": used_by
    })))
}

/// §8.3: the approval queue is the gate between "an agent wrote a Dockerfile"
/// and "that Dockerfile runs on our build fleet".
#[derive(serde::Deserialize)]
struct EgressQuery {
    status: Option<String>,
}

async fn list_egress(
    _u: AdminUser,
    State(app): State<Arc<App>>,
    Query(q): Query<EgressQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = app.store.list_egress(q.status.as_deref()).map_err(ApiError::from)?;
    Ok(Json(json!({ "egress": rows })))
}

#[derive(serde::Deserialize)]
struct EgressDecision {
    project_id: String,
    host: String,
    /// `approved` | `rejected`. Re-deciding is allowed: revoking sets the row
    /// back to unapproved, and the next task dispatched stops carrying it.
    action: String,
}

/// Approve or revoke one host for one project (§7.1).
///
/// Deliberately not a bulk endpoint. Each row widens what a sandbox can reach,
/// and §16 is clear that any reachable host is a channel a build script can
/// encode data into — so the decision is made one host at a time, by a name a
/// human read.
async fn decide_egress(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Json(req): Json<EgressDecision>,
) -> ApiResult<Json<serde_json::Value>> {
    let status = match req.action.as_str() {
        "approve" | "approved" => "approved",
        "reject" | "rejected" => "rejected",
        other => return Err(ApiError::bad(format!("unknown action `{other}`"))),
    };
    // Validated again on the way in: the stored row is what the worker will be
    // told to allow, and `*` must not become reachable because something wrote
    // it straight into the database.
    let host = rc_core::egress::normalize(&req.host).map_err(ApiError::bad)?;
    let changed = app
        .store
        .set_egress_status(&req.project_id, &host, status, &u.username)
        .map_err(ApiError::from)?;
    if changed == 0 {
        return Err(ApiError::not_found("no such egress request"));
    }
    app.store
        .audit(&u.username, &format!("egress_{status}"), &req.project_id, &host)
        .ok();
    Ok(Json(json!({ "project_id": req.project_id, "host": host, "status": status })))
}

async fn list_pre_commands(
    _u: AdminUser,
    State(app): State<Arc<App>>,
    Query(q): Query<EgressQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = app.store.list_pre_commands(q.status.as_deref()).map_err(ApiError::from)?;
    Ok(Json(json!({ "pre_commands": rows })))
}

#[derive(serde::Deserialize)]
struct PreCommandsDecision {
    project_id: String,
    #[serde(default)]
    path: String,
    /// The content digest of the exact script being decided on. Required, and
    /// not derivable from the project alone: approving "this project's
    /// pre_commands" without naming which script would silently bless whatever
    /// arrived last.
    digest: String,
    /// `approved` | `rejected`.
    action: String,
}

/// Approve or revoke one learned `pre_commands` script (§3.2).
///
/// The script is arbitrary shell that will run inside the sandbox of every
/// agent inheriting this profile, so the decision is per exact content: the
/// digest is the identity, and an edited script comes back as a new request
/// rather than riding the old approval.
async fn decide_pre_commands(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Json(req): Json<PreCommandsDecision>,
) -> ApiResult<Json<serde_json::Value>> {
    let status = match req.action.as_str() {
        "approve" | "approved" => "approved",
        "reject" | "rejected" => "rejected",
        other => return Err(ApiError::bad(format!("unknown action `{other}`"))),
    };
    let changed = app
        .store
        .set_pre_commands_status(&req.project_id, &req.path, &req.digest, status, &u.username)
        .map_err(ApiError::from)?;
    if changed == 0 {
        return Err(ApiError::not_found("no such pre_commands request"));
    }
    app.store
        .audit(
            &u.username,
            &format!("pre_commands_{status}"),
            &req.project_id,
            &req.digest,
        )
        .ok();
    Ok(Json(json!({
        "project_id": req.project_id, "path": req.path,
        "digest": req.digest, "status": status
    })))
}

async fn approve_image(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    app.store.get_image(&id).map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("unknown image"))?;
    app.store.approve_image(&id, &u.username).map_err(ApiError::from)?;
    app.store.audit(&u.username, "approve_image", &id, "").ok();
    let refreshed = app.store.get_image(&id).map_err(ApiError::from)?.unwrap_or_default();
    app.events.publish(Event::ImageUpdated {
        env_id: id.clone(),
        status: refreshed.status.clone(),
        message: format!("approved by {}", u.username),
        at: rc_core::now_ms(),
    });
    images::dispatch_pending_builds(&app).await.ok();
    Ok(Json(json!({ "ok": true, "status": refreshed.status })))
}

async fn reject_image(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    app.store
        .set_image_status(&id, "rejected", &format!("rejected by {}", u.username))
        .map_err(ApiError::from)?;
    app.store.audit(&u.username, "reject_image", &id, "").ok();
    Ok(Json(json!({ "ok": true })))
}

async fn rebuild_image(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = app
        .store
        .get_image(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("unknown image"))?;
    if row.approved_by.is_empty() && app.policy().require_image_approval {
        return Err(ApiError::bad("approve the image before rebuilding it"));
    }
    app.store
        .set_image_status(&id, "building", "rebuild requested")
        .map_err(ApiError::from)?;
    app.store.audit(&u.username, "rebuild_image", &id, "").ok();
    let n = images::dispatch_pending_builds(&app).await.map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "dispatched": n })))
}

#[derive(Deserialize)]
struct MirrorReq {
    #[serde(default)]
    worker_id: String,
    #[serde(default)]
    worker_ids: Vec<String>,
}

async fn push_image(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(req): Json<MirrorReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let worker = if req.worker_id.is_empty() {
        None
    } else {
        Some(req.worker_id.as_str())
    };
    let st = images::dispatch_push(&app, &id, worker)
        .await
        .map_err(ApiError::from)?;
    app.store
        .audit(&u.username, "push_image", &id, &st.remote_ref)
        .ok();
    Ok(Json(json!({ "ok": true, "mirror": st })))
}

async fn pull_image(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(req): Json<MirrorReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ids = if req.worker_ids.is_empty() {
        if req.worker_id.is_empty() {
            None
        } else {
            Some(vec![req.worker_id])
        }
    } else {
        Some(req.worker_ids)
    };
    let sts = images::dispatch_pull(&app, &id, ids)
        .await
        .map_err(ApiError::from)?;
    app.store
        .audit(
            &u.username,
            "pull_image",
            &id,
            &format!("{} worker(s)", sts.len()),
        )
        .ok();
    Ok(Json(json!({ "ok": true, "mirrors": sts })))
}

// ------------------------------- profiles -------------------------------

async fn list_profiles(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "profiles": app.store.list_profiles().map_err(ApiError::from)?
    })))
}

#[derive(Deserialize)]
struct ProfileUpdate {
    project_id: String,
    #[serde(default)]
    path: String,
    config_toml: String,
}

async fn update_profile(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(req): Json<ProfileUpdate>,
) -> ApiResult<Json<serde_json::Value>> {
    let parsed = rc_core::profile::parse_toml(&req.config_toml)
        .map_err(|e| ApiError::bad(format!("invalid toml: {e}")))?;
    let row = crate::store::ProfileRow {
        id,
        project_id: req.project_id,
        path: req.path,
        adapter: parsed.profile.adapter.clone().unwrap_or_default(),
        image: parsed.profile.image.clone().unwrap_or_default(),
        config_toml: req.config_toml,
        created_by: format!("admin:{}", u.username),
        ..Default::default()
    };
    app.store.upsert_profile(&row).map_err(ApiError::from)?;
    app.store.audit(&u.username, "update_profile", &row.id, "").ok();
    Ok(Json(json!({ "ok": true, "unknown_keys": parsed.unknown_keys })))
}

async fn delete_profile(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    app.store.delete_profile(&id).map_err(ApiError::from)?;
    app.store.audit(&u.username, "delete_profile", &id, "").ok();
    Ok(Json(json!({ "ok": true })))
}

async fn list_projects(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "projects": app.store.list_projects().map_err(ApiError::from)?,
        "worktrees": app.store.list_worktrees(None).map_err(ApiError::from)?,
    })))
}

// -------------------------------- storage --------------------------------

async fn storage(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let (blobs, bytes, pinned) = app.store.cas_summary().map_err(ApiError::from)?;
    let (disk_bytes, disk_blobs) = app.cas.usage();
    let policy = app.policy();
    Ok(Json(json!({
        "tracked": { "blobs": blobs, "bytes": bytes, "pinned": pinned },
        "on_disk": { "blobs": disk_blobs, "bytes": disk_bytes },
        "collectable": app.store
            .collectable_blobs(policy.blob_gc_ttl_secs, 1000)
            .map_err(ApiError::from)?
            .len(),
        "policy": { "blob_gc_ttl_secs": policy.blob_gc_ttl_secs,
                    "log_retention_secs": policy.log_retention_secs },
    })))
}

async fn run_gc(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
) -> ApiResult<Json<serde_json::Value>> {
    let report = crate::bg::collect_garbage(&app).map_err(ApiError::from)?;
    app.store.audit(&u.username, "run_gc", "", &format!("{report:?}")).ok();
    Ok(Json(json!({ "deleted": report.deleted, "bytes": report.bytes })))
}

// -------------------------------- settings --------------------------------

async fn get_settings(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "policy": app.policy(),
        "raw": app.store.all_settings().map_err(ApiError::from)?
            .into_iter()
            // Per-task scratch keys would drown the settings page.
            .filter(|(k, _)| !k.starts_with("missing_blobs:"))
            .collect::<Vec<_>>(),
    })))
}

async fn put_settings(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Json(policy): Json<Policy>,
) -> ApiResult<Json<serde_json::Value>> {
    if policy.max_infra_retries < 0 || policy.max_diagnostics == 0 {
        return Err(ApiError::bad("max_infra_retries must be >= 0 and max_diagnostics > 0"));
    }
    app.set_policy(policy).map_err(ApiError::from)?;
    app.store.audit(&u.username, "update_settings", "", "").ok();
    Ok(Json(json!({ "ok": true, "policy": app.policy() })))
}

#[derive(Deserialize)]
struct NewAdmin {
    username: String,
    password: String,
    #[serde(default)]
    role: Option<String>,
}

async fn create_admin(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Json(req): Json<NewAdmin>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.password.len() < 8 {
        return Err(ApiError::bad("password must be at least 8 characters"));
    }
    let role = req.role.unwrap_or_else(|| "viewer".into());
    let hash = auth::hash_password(&req.password).map_err(ApiError::from)?;
    app.store
        .create_admin(&req.username, &hash, &role)
        .map_err(ApiError::from)?;
    app.store.audit(&u.username, "create_admin", &req.username, &role).ok();
    Ok(Json(json!({ "ok": true })))
}

async fn delete_admin(
    AdminUser(u): AdminUser,
    State(app): State<Arc<App>>,
    Path(username): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Deleting the last admin would lock everyone out of the console.
    let admins = app.store.list_admins().map_err(ApiError::from)?;
    let remaining_admins = admins
        .iter()
        .filter(|(name, role, _)| role == "admin" && name != &username)
        .count();
    if remaining_admins == 0 {
        return Err(ApiError::bad("cannot remove the last admin account"));
    }
    app.store.delete_admin(&username).map_err(ApiError::from)?;
    app.store.audit(&u.username, "delete_admin", &username, "").ok();
    Ok(Json(json!({ "ok": true })))
}

async fn list_admins(_u: User, State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let rows = app.store.list_admins().map_err(ApiError::from)?;
    Ok(Json(json!({
        "admins": rows.iter().map(|(u, r, at)| json!({
            "username": u, "role": r, "created_at": at
        })).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default)]
    limit: Option<i64>,
}

async fn audit(
    _u: User,
    State(app): State<Arc<App>>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "entries": app.store.list_audit(q.limit.unwrap_or(200).min(1000)).map_err(ApiError::from)?
    })))
}

#[derive(Deserialize)]
struct AlertQuery {
    #[serde(default)]
    include_resolved: bool,
}

async fn alerts(
    _u: User,
    State(app): State<Arc<App>>,
    Query(q): Query<AlertQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "alerts": app.store.list_alerts(q.include_resolved).map_err(ApiError::from)?
    })))
}

/// Prometheus scrape target (§15.1 layer 2). Unauthenticated on purpose:
/// scrapers do not carry session cookies, and the admin port is expected to be
/// reachable only from inside the deployment.
async fn prometheus(State(app): State<Arc<App>>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        app.metrics.render_prometheus(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskRow;

    #[test]
    fn a_task_is_rendered_with_the_name_of_the_worker_that_ran_it() {
        let mut names = std::collections::HashMap::new();
        names.insert("w1".to_string(), "beefy-01".to_string());

        let task =
            TaskRow { id: "t1".into(), worker_id: "w1".into(), ..Default::default() };
        let v = task_json(&task, &names);
        assert_eq!(v["worker_id"], "w1", "history keeps the identity, not the alias");
        assert_eq!(v["worker_name"], "beefy-01");

        // A worker nobody renamed (or one already deleted) still serializes the
        // field, so the console has exactly one rule: empty means show the id.
        let task =
            TaskRow { id: "t2".into(), worker_id: "gone".into(), ..Default::default() };
        assert_eq!(task_json(&task, &names)["worker_name"], "");
    }

    fn test_app_with_worker(
        worker_id: &str,
    ) -> (
        Arc<App>,
        tokio::sync::mpsc::Receiver<Result<pb::ServerCmd, tonic::Status>>,
    ) {
        let dir = std::env::temp_dir().join(format!("rc-admin-{}", ulid::Ulid::generate()));
        let app = App::new(crate::config::Config {
            data_dir: dir,
            http_addr: "127.0.0.1:0".into(),
            grpc_addr: "127.0.0.1:0".into(),
            allow_anonymous_agents: true,
            session_ttl_secs: 3600,
        })
        .unwrap();
        app.store
            .upsert_worker(
                &crate::store::WorkerRow {
                    id: worker_id.into(),
                    arch: "x86_64".into(),
                    status: "online".into(),
                    version: "0.1.4".into(),
                    max_parallel: 4,
                    ..Default::default()
                },
                "tokhash",
            )
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        app.workers.connect(worker_id, "x86_64", "0.1.4", 4, tx);
        (app, rx)
    }

    fn admin_user() -> AdminUser {
        AdminUser(User {
            username: "admin".into(),
            role: Role::Admin,
        })
    }

    #[tokio::test]
    async fn cleanup_worker_runs_async_and_releases_lock_on_done() {
        let (app, mut rx) = test_app_with_worker("w1");

        let (status, Json(started)) = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: false,
                idle_days: 7,
            }),
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(started["status"], "running");
        let req_id = started["request_id"].as_str().unwrap().to_string();

        assert_eq!(app.workers.get("w1").unwrap().status, "draining");

        // Second concurrent trigger must be rejected with 409 CONFLICT.
        let err = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: false,
                idle_days: 7,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);

        // Worker receives CleanupOrder and replies with CleanupDone.
        let cmd = rx.recv().await.unwrap().unwrap();
        let Some(pb::server_cmd::Body::Cleanup(order)) = cmd.body else {
            panic!("expected CleanupOrder");
        };
        assert_eq!(order.request_id, req_id);

        app.complete_cleanup_from_worker(
            "w1",
            pb::CleanupDone {
            request_id: req_id.clone(),
            ok: true,
            message: "reclaimed 2".into(),
            reclaimed: 2,
            skipped_active: 0,
            skipped_fresh: 1,
            disk_free_gb_before: 10,
            disk_free_gb_after: 20,
            reclaimed_worktrees: vec!["wt1".into(), "wt2".into()],
        });

        // Background watcher settles quickly.
        let mut settled = None;
        for _ in 0..50 {
            let job = app.get_cleanup_job("w1", &req_id).unwrap();
            if job.status != "running" {
                settled = Some(job);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let job = settled.expect("cleanup job did not settle");
        assert_eq!(job.status, "done");
        assert!(job.ok);
        assert_eq!(job.reclaimed, 2);
        assert_eq!(app.workers.get("w1").unwrap().status, "online");
        assert!(!app.workers.get("w1").unwrap().admin_drain);
        assert!(app.worker_cleanup_summary("w1").0.is_none());
    }

    #[tokio::test]
    async fn cleanup_guard_drop_releases_lock_and_restores_status() {
        let (app, _rx) = test_app_with_worker("w1");
        let conn_gen = app.workers.get("w1").unwrap().conn_gen;
        let _rx_wait = app
            .try_begin_cleanup("w1", "req-drop", conn_gen, false, 7)
            .unwrap();
        app.workers.set_status("w1", "draining");

        {
            let _guard = CleanupGuard {
                app: app.clone(),
                worker_id: "w1".into(),
                request_id: "req-drop".into(),
                conn_gen,
                prior_status: "online".into(),
                prior_was_draining: false,
                armed: true,
            };
        }

        let job = app.get_cleanup_job("w1", "req-drop").unwrap();
        assert_eq!(job.status, "failed");
        assert!(app.worker_cleanup_summary("w1").0.is_none());
        assert_eq!(app.workers.get("w1").unwrap().status, "online");
        assert!(!app.workers.get("w1").unwrap().admin_drain);
    }

    #[tokio::test]
    async fn cleanup_releases_lock_immediately_on_worker_disconnect() {
        let (app, mut rx) = test_app_with_worker("w1");
        let conn_gen = app.workers.get("w1").unwrap().conn_gen;

        let (_status, Json(started)) = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: true,
                idle_days: 0,
            }),
        )
        .await
        .unwrap();
        let req_id = started["request_id"].as_str().unwrap().to_string();
        let _ = rx.recv().await;

        app.workers.disconnect_if_gen("w1", conn_gen);
        app.fail_inflight_cleanup_for_gen("w1", conn_gen, "worker disconnected during cleanup");

        let job = app.get_cleanup_job("w1", &req_id).unwrap();
        assert_eq!(job.status, "failed");
        assert!(!job.ok);
        assert!(app.worker_cleanup_summary("w1").0.is_none());
    }

    #[tokio::test]
    async fn stale_watcher_and_disconnect_cannot_clobber_reconnected_cleanup_b() {
        // Barrier-ordered sequence:
        // 1. Cleanup A starts on gen1 (draining).
        // 2. Worker disconnects (gen1), reconnects (gen2).
        // 3. Cleanup B starts on gen2 (draining).
        // 4. Stale disconnect/watcher for A (gen1) finishes after B started.
        // Assert: Cleanup B stays running, inflight, and worker remains draining.
        let (app, _rx1) = test_app_with_worker("w1");
        let gen1 = app.workers.get("w1").unwrap().conn_gen;
        let rx_a = app
            .try_begin_cleanup("w1", "req-a", gen1, false, 7)
            .unwrap();
        assert!(app.workers.set_status_if_gen("w1", gen1, "draining"));
        let guard_a = CleanupGuard {
            app: app.clone(),
            worker_id: "w1".into(),
            request_id: "req-a".into(),
            conn_gen: gen1,
            prior_status: "online".into(),
            prior_was_draining: false,
            armed: true,
        };

        // Disconnect gen1 and reconnect as gen2.
        assert!(app.workers.disconnect_if_gen("w1", gen1).is_some());
        app.fail_inflight_cleanup_for_gen("w1", gen1, "worker disconnected during cleanup");
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(8);
        let gen2 = app.workers.connect("w1", "x86_64", "0.1.4", 4, tx2);
        assert_ne!(gen1, gen2);

        // Start Cleanup B on gen2 via the real handler.
        let (_status, Json(started_b)) = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: false,
                idle_days: 3,
            }),
        )
        .await
        .unwrap();
        let req_b = started_b["request_id"].as_str().unwrap().to_string();
        let _ = rx2.recv().await;
        assert_eq!(app.workers.get("w1").unwrap().status, "draining");

        // Now run A's watcher to completion (rx_a already closed) AND simulate a
        // late stale disconnect_if_gen/fail_inflight_cleanup_for_gen for gen1.
        watch_cleanup(
            rx_a,
            guard_a,
            "admin".into(),
            false,
            7,
            std::time::Duration::from_secs(10),
        )
        .await;
        assert!(app.workers.disconnect_if_gen("w1", gen1).is_none());
        app.fail_inflight_cleanup_for_gen("w1", gen1, "stale disconnect");

        // Cleanup B and gen2 draining pin must be completely untouched.
        let w = app.workers.get("w1").unwrap();
        assert_eq!(w.conn_gen, gen2);
        assert_eq!(w.status, "draining");
        assert!(w.admin_drain);
        let (active, _) = app.worker_cleanup_summary("w1");
        assert_eq!(active.unwrap().request_id, req_b);
        assert_eq!(app.get_cleanup_job("w1", "req-a").unwrap().status, "failed");
    }

    #[tokio::test]
    async fn consecutive_cleanups_restore_then_drain_cleanly() {
        let (app, mut rx) = test_app_with_worker("w1");

        let (_s1, Json(j1)) = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: false,
                idle_days: 7,
            }),
        )
        .await
        .unwrap();
        let r1 = j1["request_id"].as_str().unwrap().to_string();
        let _ = rx.recv().await;

        app.complete_cleanup_from_worker(
            "w1",
            pb::CleanupDone {
            request_id: r1.clone(),
            ok: true,
            message: "pass 1".into(),
            ..Default::default()
        });
        for _ in 0..50 {
            if app.worker_cleanup_summary("w1").0.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(app.workers.get("w1").unwrap().status, "online");

        // Immediately launch cleanup 2.
        let (_s2, Json(j2)) = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: true,
                idle_days: 0,
            }),
        )
        .await
        .unwrap();
        let r2 = j2["request_id"].as_str().unwrap().to_string();
        let _ = rx.recv().await;
        assert_eq!(app.workers.get("w1").unwrap().status, "draining");
        assert_eq!(app.worker_cleanup_summary("w1").0.unwrap().request_id, r2);
    }

    #[tokio::test]
    async fn late_cleanup_done_does_not_overwrite_timed_out_failed_job() {
        let (app, _rx) = test_app_with_worker("w1");
        let conn_gen = app.workers.get("w1").unwrap().conn_gen;
        let rx = app
            .try_begin_cleanup("w1", "req-timeout", conn_gen, false, 7)
            .unwrap();
        assert!(app.workers.set_status_if_gen("w1", conn_gen, "draining"));
        let guard = CleanupGuard {
            app: app.clone(),
            worker_id: "w1".into(),
            request_id: "req-timeout".into(),
            conn_gen,
            prior_status: "online".into(),
            prior_was_draining: false,
            armed: true,
        };

        watch_cleanup(
            rx,
            guard,
            "admin".into(),
            false,
            7,
            std::time::Duration::from_millis(10),
        )
        .await;

        let timed_out = app.get_cleanup_job("w1", "req-timeout").unwrap();
        assert_eq!(timed_out.status, "failed");
        assert!(!timed_out.ok);
        let original_finished_at = timed_out.finished_at;

        // Worker sends late CleanupDone after timeout.
        app.complete_cleanup_from_worker(
            "w1",
            pb::CleanupDone {
            request_id: "req-timeout".into(),
            ok: true,
            message: "late success".into(),
            reclaimed: 5,
            ..Default::default()
        });

        let after_late = app.get_cleanup_job("w1", "req-timeout").unwrap();
        assert_eq!(after_late.status, "failed");
        assert!(!after_late.ok);
        assert_eq!(after_late.reclaimed, 0);
        assert_eq!(after_late.finished_at, original_finished_at);
    }

    #[test]
    fn cleanup_job_history_is_hard_capped_across_many_workers() {
        let (app, _rx) = test_app_with_worker("w-seed");
        // Keep one running job alive; pruning must never evict active jobs,
        // while completed history across >128 distinct workers is hard-capped.
        let _active_rx = app
            .try_begin_cleanup("w-active", "req-active", 1, false, 7)
            .unwrap();
        for i in 0..200 {
            let wid = format!("w-{i:03}");
            let req = format!("req-{i:03}");
            let _rx = app.try_begin_cleanup(&wid, &req, 1, false, 7).unwrap();
            app.settle_cleanup_and_restore(
                &wid,
                &req,
                1,
                CleanupOutcome::Done(pb::CleanupDone {
                    request_id: req.clone(),
                    ok: true,
                    message: "ok".into(),
                    reclaimed: 1,
                    ..Default::default()
                }),
                "online",
                false,
            );
        }
        let (jobs_len, latest_len) = app.retained_cleanup_counts();
        assert_eq!(jobs_len, crate::app::MAX_RETAINED_CLEANUP_JOBS + 1);
        assert_eq!(latest_len, crate::app::MAX_RETAINED_CLEANUP_JOBS + 1);
        assert_eq!(
            app.get_cleanup_job("w-active", "req-active")
                .unwrap()
                .status,
            "running"
        );
    }

    #[tokio::test]
    async fn reconnect_before_old_teardown_fails_old_cleanup_and_unblocks_new_one() {
        let (app, _rx1) = test_app_with_worker("w1");
        let gen1 = app.workers.get("w1").unwrap().conn_gen;

        let (_status_a, Json(started_a)) = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: false,
                idle_days: 7,
            }),
        )
        .await
        .unwrap();
        let req_a = started_a["request_id"].as_str().unwrap().to_string();

        // Worker reconnects as gen2 BEFORE gen1 stream teardown runs.
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(8);
        let gen2 = app.workers.connect("w1", "x86_64", "0.1.4", 4, tx2);
        app.fail_inflight_cleanups_before_gen("w1", gen2, "worker reconnected during cleanup");

        // Cleanup B on gen2 succeeds immediately instead of hitting 409 CONFLICT.
        let (_status_b, Json(started_b)) = cleanup_worker(
            admin_user(),
            State(app.clone()),
            Path("w1".into()),
            Json(CleanupReq {
                all_unused: true,
                idle_days: 0,
            }),
        )
        .await
        .unwrap();
        let req_b = started_b["request_id"].as_str().unwrap().to_string();
        let _ = rx2.recv().await;

        // Now gen1's teardown runs late: it must neither disconnect gen2 nor abort B.
        app.fail_inflight_cleanup_for_gen("w1", gen1, "worker disconnected during cleanup");
        assert!(app.workers.disconnect_if_gen("w1", gen1).is_none());

        // Also yield so A's spawned watch_cleanup finishes settling.
        tokio::task::yield_now().await;

        assert_eq!(app.get_cleanup_job("w1", &req_a).unwrap().status, "failed");
        let (active, _) = app.worker_cleanup_summary("w1");
        assert_eq!(active.unwrap().request_id, req_b);
        let w = app.workers.get("w1").unwrap();
        assert_eq!(w.conn_gen, gen2);
        assert_eq!(w.status, "draining");
        assert!(w.admin_drain);
    }

    #[test]
    fn complete_cleanup_from_worker_ignores_foreign_worker_reply() {
        let (app, _rx) = test_app_with_worker("w1");
        let conn_gen = app.workers.get("w1").unwrap().conn_gen;
        let _rx_wait = app
            .try_begin_cleanup("w1", "req-w1", conn_gen, false, 7)
            .unwrap();

        app.complete_cleanup_from_worker(
            "w2",
            pb::CleanupDone {
                request_id: "req-w1".into(),
                ok: true,
                message: "spoofed".into(),
                reclaimed: 99,
                ..Default::default()
            },
        );

        let job = app.get_cleanup_job("w1", "req-w1").unwrap();
        assert_eq!(job.status, "running");
        assert_eq!(
            app.worker_cleanup_summary("w1").0.unwrap().request_id,
            "req-w1"
        );
    }
}
