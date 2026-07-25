//! Live worker registry.
//!
//! Heartbeat stats stay here, in memory, and never hit SQLite at heartbeat
//! frequency (§15.1, risk #28). The database only holds enrollment facts and a
//! low-frequency `last_hb` stamp.

use parking_lot::Mutex;
use rc_core::pb::{ServerCmd, WorkerStats};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

pub type CmdSender = mpsc::Sender<Result<ServerCmd, tonic::Status>>;

#[derive(Clone)]
pub struct WorkerConn {
    pub id: String,
    pub arch: String,
    pub version: String,
    pub status: String,
    pub max_parallel: u32,
    pub stats: WorkerStats,
    pub last_hb_ms: i64,
    pub connected_at: i64,
    /// Optional features this worker reports, refreshed on every heartbeat.
    /// Enrollment happens once, so a worker upgraded in place would never
    /// update a capability list recorded there.
    pub capabilities: HashSet<String>,
    tx: CmdSender,
    /// Tasks the scheduler has handed to this worker but which have not yet
    /// reported a terminal state.
    pub assigned: HashSet<String>,
}

impl WorkerConn {
    /// Slots left, counting work the scheduler has already committed but the
    /// worker has not yet reflected in its heartbeat.
    pub fn free_slots(&self) -> u32 {
        let busy = self.assigned.len().max(self.stats.running_tasks as usize) as u32;
        self.max_parallel.saturating_sub(busy)
    }
}

#[derive(Clone, Default)]
pub struct WorkerRegistry {
    inner: Arc<Mutex<HashMap<String, WorkerConn>>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn connect(
        &self,
        id: &str,
        arch: &str,
        version: &str,
        max_parallel: u32,
        tx: CmdSender,
    ) {
        let now = rc_core::now_ms();
        let mut g = self.inner.lock();
        g.insert(
            id.to_string(),
            WorkerConn {
                id: id.to_string(),
                arch: arch.to_string(),
                version: version.to_string(),
                status: "online".into(),
                max_parallel: max_parallel.max(1),
                stats: WorkerStats::default(),
                last_hb_ms: now,
                connected_at: now,
                // Empty until a heartbeat says otherwise, so a worker whose
                // capabilities are not yet known is treated as having none.
                capabilities: HashSet::new(),
                tx,
                assigned: HashSet::new(),
            },
        );
    }

    pub fn disconnect(&self, id: &str) -> Vec<String> {
        let mut g = self.inner.lock();
        g.remove(id)
            .map(|w| w.assigned.into_iter().collect())
            .unwrap_or_default()
    }

    pub fn heartbeat(
        &self,
        id: &str,
        stats: WorkerStats,
        status: &str,
        active: &[String],
        capabilities: &[String],
    ) {
        let mut g = self.inner.lock();
        if let Some(w) = g.get_mut(id) {
            w.stats = stats;
            w.status = status.to_string();
            w.last_hb_ms = rc_core::now_ms();
            w.capabilities = capabilities.iter().cloned().collect();
            // Reconcile against what the worker says it is actually running:
            // a task the worker has forgotten must not hold a slot forever.
            let active: HashSet<String> = active.iter().cloned().collect();
            w.assigned.retain(|t| active.contains(t));
        }
    }

    pub fn note_assigned(&self, id: &str, task_id: &str) {
        if let Some(w) = self.inner.lock().get_mut(id) {
            w.assigned.insert(task_id.to_string());
        }
    }

    pub fn note_finished(&self, id: &str, task_id: &str) {
        if let Some(w) = self.inner.lock().get_mut(id) {
            w.assigned.remove(task_id);
        }
    }

    pub fn get(&self, id: &str) -> Option<WorkerConn> {
        self.inner.lock().get(id).cloned()
    }

    pub fn sender(&self, id: &str) -> Option<CmdSender> {
        self.inner.lock().get(id).map(|w| w.tx.clone())
    }

    pub fn is_connected(&self, id: &str) -> bool {
        self.inner.lock().contains_key(id)
    }

    pub fn snapshot(&self) -> Vec<WorkerConn> {
        let mut v: Vec<WorkerConn> = self.inner.lock().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn set_status(&self, id: &str, status: &str) {
        if let Some(w) = self.inner.lock().get_mut(id) {
            w.status = status.to_string();
        }
    }

    pub fn online_count(&self) -> usize {
        self.inner.lock().values().filter(|w| w.status == "online").count()
    }

    pub fn total_running(&self) -> u32 {
        self.inner.lock().values().map(|w| w.stats.running_tasks).sum()
    }

    /// Deliver a command, dropping the worker if its channel is gone.
    pub async fn send(&self, id: &str, cmd: ServerCmd) -> bool {
        let Some(tx) = self.sender(id) else {
            return false;
        };
        match tx.send(Ok(cmd)).await {
            Ok(()) => true,
            Err(_) => {
                self.disconnect(id);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::pb::server_cmd::Body;

    fn registry_with(id: &str, max_parallel: u32) -> (WorkerRegistry, mpsc::Receiver<Result<ServerCmd, tonic::Status>>) {
        let (tx, rx) = mpsc::channel(8);
        let r = WorkerRegistry::new();
        r.connect(id, "x86_64", "0.1.0", max_parallel, tx);
        (r, rx)
    }

    #[tokio::test]
    async fn commands_reach_the_worker() {
        let (r, mut rx) = registry_with("w1", 2);
        assert!(
            r.send("w1", ServerCmd { body: Some(Body::CancelTaskId("t1".into())) })
                .await
        );
        let got = rx.recv().await.unwrap().unwrap();
        assert!(matches!(got.body, Some(Body::CancelTaskId(t)) if t == "t1"));
    }

    #[tokio::test]
    async fn a_dead_channel_evicts_the_worker() {
        let (r, rx) = registry_with("w1", 2);
        drop(rx);
        assert!(!r.send("w1", ServerCmd { body: Some(Body::Ping(Default::default())) }).await);
        assert!(!r.is_connected("w1"));
    }

    #[tokio::test]
    async fn assigned_tasks_consume_slots_before_the_next_heartbeat() {
        // Without this, a burst of submissions all land on the same worker in
        // the window before its heartbeat catches up.
        let (r, _rx) = registry_with("w1", 2);
        assert_eq!(r.get("w1").unwrap().free_slots(), 2);
        r.note_assigned("w1", "t1");
        assert_eq!(r.get("w1").unwrap().free_slots(), 1);
        r.note_assigned("w1", "t2");
        assert_eq!(r.get("w1").unwrap().free_slots(), 0);
        r.note_finished("w1", "t1");
        assert_eq!(r.get("w1").unwrap().free_slots(), 1);
    }

    #[tokio::test]
    async fn heartbeat_reconciles_forgotten_assignments() {
        let (r, _rx) = registry_with("w1", 4);
        r.note_assigned("w1", "t1");
        r.note_assigned("w1", "ghost");
        r.heartbeat("w1", WorkerStats::default(), "online", &["t1".into()], &[]);
        let w = r.get("w1").unwrap();
        assert!(w.assigned.contains("t1"));
        assert!(!w.assigned.contains("ghost"));
    }

    #[tokio::test]
    async fn capabilities_arrive_with_the_heartbeat_and_are_replaced_each_time() {
        // Enrollment happens once, so a worker upgraded in place must be able
        // to announce what it can do now — and a downgrade must be seen too.
        let (r, _rx) = registry_with("w1", 4);
        assert!(r.get("w1").unwrap().capabilities.is_empty());

        r.heartbeat("w1", WorkerStats::default(), "online", &[], &["multi-root".into()]);
        assert!(r.get("w1").unwrap().capabilities.contains("multi-root"));

        r.heartbeat("w1", WorkerStats::default(), "online", &[], &[]);
        assert!(r.get("w1").unwrap().capabilities.is_empty());
    }

    #[tokio::test]
    async fn disconnect_returns_orphaned_tasks_for_requeue() {
        let (r, _rx) = registry_with("w1", 4);
        r.note_assigned("w1", "t1");
        let orphans = r.disconnect("w1");
        assert_eq!(orphans, vec!["t1"]);
        assert!(!r.is_connected("w1"));
    }
}
