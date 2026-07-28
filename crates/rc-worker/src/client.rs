//! gRPC client for the control plane (§13).

use anyhow::{anyhow, Context, Result};
use rc_core::cas;
use rc_core::pb::worker_api_client::WorkerApiClient;
use rc_core::pb::*;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::{Request, Status, Streaming};

#[derive(Clone)]
pub struct ServerClient {
    inner: WorkerApiClient<Channel>,
    worker_id: String,
    token: String,
}

impl ServerClient {
    pub async fn connect(server: &str, worker_id: &str, token: &str) -> Result<Self> {
        let channel = rc_core::transport::endpoint(server)?
            .connect()
            .await
            .with_context(|| format!("connect to control plane at {server}"))?;
        Ok(ServerClient {
            inner: WorkerApiClient::new(channel),
            worker_id: worker_id.to_string(),
            token: token.to_string(),
        })
    }

    /// Exchange a single-use enrollment token for a durable worker token
    /// (§8.1). No worker credentials exist yet, so this call is unauthorised
    /// by design.
    pub async fn enroll(server: &str, req: EnrollReq) -> Result<EnrollResp> {
        let channel = rc_core::transport::endpoint(server)?
            .connect()
            .await
            .with_context(|| format!("connect to control plane at {server}"))?;
        let mut client = WorkerApiClient::new(channel);
        Ok(client.enroll(Request::new(req)).await?.into_inner())
    }

    fn authed<T>(&self, message: T) -> Request<T> {
        let mut req = Request::new(message);
        if let Ok(v) = format!("Bearer {}", self.token).parse() {
            req.metadata_mut().insert("authorization", v);
        }
        if let Ok(v) = self.worker_id.parse() {
            req.metadata_mut().insert("x-worker-id", v);
        }
        req
    }

    /// Open the long-lived bidirectional channel. Events flow up through
    /// `events`; commands come back on the returned stream.
    pub async fn open_channel(
        &mut self,
        events: mpsc::Receiver<WorkerEvent>,
    ) -> Result<Streaming<ServerCmd>> {
        let stream = ReceiverStream::new(events);
        let resp = self
            .inner
            .channel(self.authed(stream))
            .await
            .context("open the worker channel")?;
        Ok(resp.into_inner())
    }

    /// Read a blob. `Ok(None)` means the control plane no longer has it — the
    /// caller reports `blob_missing` and the agent re-uploads (§4.7). That is
    /// a recoverable race, not a task failure.
    pub async fn fetch_blob(&mut self, hash: &str) -> Result<Option<Vec<u8>>> {
        let req = self.authed(BlobReq {
            hash: hash.to_string(),
        });
        let mut stream = match self.inner.fetch_blob(req).await {
            Ok(r) => r.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
            Err(e) => return Err(e).context("fetch blob"),
        };
        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) => data.extend_from_slice(&c.data),
                Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
                Err(e) => return Err(e).context("read blob stream"),
            }
        }
        // Content addressing is only worth anything if it is checked.
        let actual = cas::hash_bytes(&data);
        if actual != hash {
            return Err(anyhow!(
                "blob {hash} arrived corrupted (hashes to {actual})"
            ));
        }
        Ok(Some(data))
    }

    pub async fn put_blob(&mut self, data: Vec<u8>) -> Result<String> {
        let hash = cas::hash_bytes(&data);
        let chunks: Vec<BlobChunk> = if data.is_empty() {
            vec![BlobChunk {
                hash: hash.clone(),
                data: vec![],
                last: true,
                total_size: 0,
            }]
        } else {
            let total = data.len() as u64;
            data.chunks(cas::CHUNK_SIZE)
                .enumerate()
                .map(|(i, c)| BlobChunk {
                    hash: hash.clone(),
                    data: c.to_vec(),
                    last: (i + 1) * cas::CHUNK_SIZE >= data.len(),
                    total_size: total,
                })
                .collect()
        };
        let stream = tokio_stream::iter(chunks);
        let resp = self
            .inner
            .put_blob(self.authed(stream))
            .await
            .context("upload blob")?
            .into_inner();
        Ok(resp.hash)
    }
}

/// Convenience wrappers so call sites read as intent, not protobuf shape.
pub fn heartbeat_event(stats: WorkerStats, status: &str, active: Vec<String>) -> WorkerEvent {
    WorkerEvent {
        body: Some(worker_event::Body::Heartbeat(Heartbeat {
            stats: Some(stats),
            active_task_ids: active,
            status: status.to_string(),
            // On every heartbeat, not just at enrollment: enrollment happens
            // once, so a worker upgraded in place would otherwise be judged
            // forever by what it could do the day it joined.
            capabilities: rc_core::CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        })),
    }
}

pub fn progress_event(task_id: &str, phase: &str, detail: &str) -> WorkerEvent {
    WorkerEvent {
        body: Some(worker_event::Body::Progress(TaskProgress {
            task_id: task_id.to_string(),
            phase: phase.to_string(),
            detail: detail.to_string(),
            ..Default::default()
        })),
    }
}

pub fn done_event(done: TaskDone) -> WorkerEvent {
    WorkerEvent {
        body: Some(worker_event::Body::Done(done)),
    }
}

pub fn image_done_event(done: ImageBuildDone) -> WorkerEvent {
    WorkerEvent {
        body: Some(worker_event::Body::ImageDone(done)),
    }
}

/// A task that failed for infrastructure reasons. The control plane retries it
/// elsewhere and the agent never sees it unless retries run out (§3.5/§6.2).
pub fn infra_failure(task_id: &str, message: impl std::fmt::Display) -> TaskDone {
    TaskDone {
        task_id: task_id.to_string(),
        result: Some(TaskResult {
            kind: rc_core::ResultKind::InfraError.as_str().to_string(),
            summary: message.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The control plane lost a blob we need; ask for a re-upload rather than
/// failing (§4.7).
pub fn blob_missing(task_id: &str, missing: Vec<String>) -> TaskDone {
    TaskDone {
        task_id: task_id.to_string(),
        missing_blobs: missing,
        ..Default::default()
    }
}

pub fn is_transient(status: &Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Aborted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infra_failures_are_tagged_so_the_agent_is_not_told_to_edit_code() {
        let done = infra_failure("t1", "docker daemon unreachable");
        assert_eq!(done.result.unwrap().kind, "infra_error");
    }

    #[test]
    fn a_blob_missing_report_carries_no_result() {
        // A result would look like a verdict on the code; this is a resync
        // request (§4.7).
        let done = blob_missing("t1", vec!["a".repeat(64)]);
        assert!(done.result.is_none());
        assert_eq!(done.missing_blobs.len(), 1);
    }

    #[test]
    fn transient_status_codes_are_worth_retrying() {
        assert!(is_transient(&Status::unavailable("restarting")));
        assert!(!is_transient(&Status::unauthenticated("bad token")));
        assert!(!is_transient(&Status::invalid_argument("nope")));
    }

    #[test]
    fn heartbeats_carry_the_active_task_set_for_reconciliation() {
        let ev = heartbeat_event(WorkerStats::default(), "online", vec!["t1".into()]);
        match ev.body.unwrap() {
            worker_event::Body::Heartbeat(hb) => {
                assert_eq!(hb.active_task_ids, vec!["t1"]);
                assert_eq!(hb.status, "online");
            }
            _ => panic!("wrong event body"),
        }
    }
}
