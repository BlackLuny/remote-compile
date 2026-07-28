//! `WorkerApi` — enrollment plus the long-lived bidirectional channel every
//! worker holds open (§13).

use crate::app::App;
use crate::auth;
use crate::events::Event;
use crate::images;
use rc_core::cas;
use rc_core::pb::worker_api_server::{WorkerApi, WorkerApiServer};
use rc_core::pb::*;
use rc_core::{ids, now_ms};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

pub struct WorkerService {
    app: Arc<App>,
}

type CmdStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ServerCmd, Status>> + Send>>;
type BlobStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<BlobChunk, Status>> + Send>>;

impl WorkerService {
    pub fn new(app: Arc<App>) -> WorkerApiServer<Self> {
        WorkerApiServer::new(WorkerService { app })
    }

    /// Every call after enrollment carries the issued worker token.
    fn authenticate<T>(&self, req: &Request<T>) -> Result<String, Status> {
        let token = auth::bearer_from_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing worker token"))?;
        let worker_id = req
            .metadata()
            .get("x-worker-id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Status::unauthenticated("missing x-worker-id"))?
            .to_string();
        let expected = self
            .app
            .store
            .worker_token_hash(&worker_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::unauthenticated("unknown worker"))?;
        if expected != auth::hash_token(&token) {
            return Err(Status::unauthenticated("worker token mismatch"));
        }
        Ok(worker_id)
    }
}

#[tonic::async_trait]
impl WorkerApi for WorkerService {
    /// Exchange a single-use enrollment token for a durable worker token
    /// (§8.1).
    async fn enroll(&self, req: Request<EnrollReq>) -> Result<Response<EnrollResp>, Status> {
        let req = req.into_inner();
        let worker_id = if req.worker_id.is_empty() {
            ids::worker_id()
        } else {
            req.worker_id.clone()
        };
        if !self
            .app
            .store
            .consume_enrollment_token(&req.enrollment_token, &worker_id)
            .map_err(|e| Status::internal(e.to_string()))?
        {
            return Err(Status::unauthenticated(
                "enrollment token is invalid, expired or already used",
            ));
        }
        let token = ids::random_token();
        let row = crate::store::WorkerRow {
            id: worker_id.clone(),
            arch: req.arch.clone(),
            labels: serde_json::to_string(&req.labels).unwrap_or_else(|_| "{}".into()),
            capacity: serde_json::json!({
                "cpu": req.cpu, "mem_gb": req.mem_gb, "disk_gb": req.disk_gb
            })
            .to_string(),
            status: "offline".into(),
            version: req.version.clone(),
            max_parallel: req.max_parallel.max(1) as i64,
            ..Default::default()
        };
        self.app
            .store
            .upsert_worker(&row, &auth::hash_token(&token))
            .map_err(|e| Status::internal(e.to_string()))?;
        self.app
            .store
            .audit("worker", "enroll", &worker_id, &req.version)
            .ok();
        tracing::info!(worker = %worker_id, version = %req.version, "worker enrolled");
        Ok(Response::new(EnrollResp {
            worker_id,
            worker_token: token,
        }))
    }

    type ChannelStream = CmdStream;

    /// The worker opens this once and keeps it open: heartbeats and results
    /// flow up, assignments and control commands flow down.
    async fn channel(
        &self,
        req: Request<Streaming<WorkerEvent>>,
    ) -> Result<Response<Self::ChannelStream>, Status> {
        let worker_id = self.authenticate(&req)?;
        let stored = self
            .app
            .store
            .list_workers()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .find(|w| w.id == worker_id)
            .ok_or_else(|| Status::not_found("worker not enrolled"))?;

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        self.app.workers.connect(
            &worker_id,
            &stored.arch,
            &stored.version,
            stored.max_parallel as u32,
            tx,
        );
        self.app.store.touch_worker(&worker_id, "online").ok();
        self.app.store.resolve_alert(&format!("worker_offline:{worker_id}")).ok();
        tracing::info!(worker = %worker_id, "worker channel opened");
        self.app.dispatch_signal.notify_one();

        let app = self.app.clone();
        let id = worker_id.clone();
        let mut inbound = req.into_inner();
        tokio::spawn(async move {
            while let Some(event) = inbound.next().await {
                let event = match event {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(worker = %id, error = %e, "worker stream error");
                        break;
                    }
                };
                if let Err(e) = handle_event(&app, &id, event).await {
                    tracing::error!(worker = %id, error = %e, "failed to handle worker event");
                }
            }
            // The channel dropped: its tasks must go back to the queue or they
            // would sit in `running` forever.
            tracing::info!(worker = %id, "worker channel closed");
            app.workers.disconnect(&id);
            app.store.touch_worker(&id, "offline").ok();
            app.reclaim_worker_tasks(&id).ok();
            app.events.publish(Event::WorkerUpdated {
                worker_id: id.clone(),
                status: "offline".into(),
                cpu_load: 0.0,
                disk_free_gb: 0,
                running_tasks: 0,
                at: now_ms(),
            });
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as CmdStream))
    }

    type FetchBlobStream = BlobStream;

    async fn fetch_blob(&self, req: Request<BlobReq>) -> Result<Response<Self::FetchBlobStream>, Status> {
        self.authenticate(&req)?;
        let hash = req.into_inner().hash;
        if !cas::is_valid_hash(&hash) {
            return Err(Status::invalid_argument("malformed hash"));
        }
        // A 404 here is the worker's cue to report `blob_missing`, which the
        // control plane turns into a re-upload request rather than a failure
        // (§4.7).
        let data = self
            .app
            .cas
            .get(&hash)
            .map_err(|_| Status::not_found(format!("blob {hash} is not in the CAS")))?;
        self.app.store.touch_blobs(&[(hash.clone(), data.len() as i64)]).ok();

        let stream = async_stream::stream! {
            let total = data.len() as u64;
            let mut offset = 0usize;
            while offset < data.len() {
                let end = (offset + cas::CHUNK_SIZE).min(data.len());
                yield Ok(BlobChunk {
                    hash: hash.clone(),
                    data: data[offset..end].to_vec(),
                    last: end == data.len(),
                    total_size: total,
                });
                offset = end;
            }
            if total == 0 {
                yield Ok(BlobChunk { hash: hash.clone(), data: vec![], last: true, total_size: 0 });
            }
        };
        Ok(Response::new(Box::pin(stream) as BlobStream))
    }

    /// Workers upload build logs (and image build logs) as CAS blobs.
    async fn put_blob(&self, req: Request<Streaming<BlobChunk>>) -> Result<Response<PutBlobResp>, Status> {
        self.authenticate(&req)?;
        let mut stream = req.into_inner();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?.data);
        }
        let hash = self
            .app
            .cas
            .put(&buf)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.app.store.touch_blobs(&[(hash.clone(), buf.len() as i64)]).ok();
        Ok(Response::new(PutBlobResp {
            hash,
            size: buf.len() as u64,
        }))
    }
}

async fn handle_event(app: &Arc<App>, worker_id: &str, event: WorkerEvent) -> anyhow::Result<()> {
    let Some(body) = event.body else { return Ok(()) };
    match body {
        worker_event::Body::Heartbeat(hb) => {
            let stats = hb.stats.clone().unwrap_or_default();
            app.workers
                .heartbeat(worker_id, stats.clone(), &hb.status, &hb.active_task_ids, &hb.capabilities);
            // Only a cheap timestamp reaches SQLite; the stats stay in memory
            // (§15.1, risk #28).
            app.store.touch_worker(worker_id, &hb.status).ok();
            app.events.publish(Event::WorkerUpdated {
                worker_id: worker_id.to_string(),
                status: hb.status.clone(),
                cpu_load: stats.cpu_load,
                disk_free_gb: stats.disk_free_gb,
                running_tasks: stats.running_tasks,
                at: now_ms(),
            });
            app.dispatch_signal.notify_one();
        }
        worker_event::Body::Progress(p) => {
            // Unit progress: phase empty + units_seen/current_unit set.
            // Must not write task_events or set_status (R7 / F26.3).
            let is_unit = p.phase.is_empty()
                && (p.units_seen > 0 || !p.current_unit.is_empty());
            if is_unit {
                if app.policy().unit_progress {
                    app.update_progress(&p.task_id, &p.current_unit, p.units_seen);
                    app.publish_task(&p.task_id);
                }
                // kill-switch off: drop silently; worker may still send.
                return Ok(());
            }
            // Real phase transitions only.
            if !p.phase.is_empty() {
                app.store
                    .add_timeline(&p.task_id, &p.phase, worker_id, &p.detail)?;
                app.store
                    .set_status(&p.task_id, &normalize_phase(&p.phase))?;
                app.publish_task(&p.task_id);
            }
        }
        worker_event::Body::Done(done) => {
            app.on_task_done(worker_id, done).await?;
        }
        worker_event::Body::ImageDone(done) => {
            images::on_build_done(app, &done)?;
        }
    }
    Ok(())
}

/// Worker phases map onto the task states the agent and console understand.
fn normalize_phase(phase: &str) -> String {
    match phase {
        "syncing" | "fetching" | "rebuilding" => "syncing",
        "building" | "running" | "pre_commands" => "running",
        "uploading" => "uploading",
        _ => "running",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_phases_map_onto_task_states() {
        assert_eq!(normalize_phase("fetching"), "syncing");
        assert_eq!(normalize_phase("pre_commands"), "running");
        assert_eq!(normalize_phase("uploading"), "uploading");
        // Unknown phases from a newer worker must not desync the state machine.
        assert_eq!(normalize_phase("brand_new_phase"), "running");
    }
}
