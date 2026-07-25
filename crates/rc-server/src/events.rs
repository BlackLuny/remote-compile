//! In-process event bus.
//!
//! Two consumers: the admin SSE stream (§14.1) and agent long-polling. A
//! broadcast channel is deliberate — a slow subscriber lags and drops old
//! events rather than blocking task execution.

use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TaskUpdated {
        task_id: String,
        status: String,
        result_kind: String,
        task_type: String,
        project_id: String,
        worktree_id: String,
        worker_id: String,
        at: i64,
    },
    WorkerUpdated {
        worker_id: String,
        status: String,
        cpu_load: f64,
        disk_free_gb: u64,
        running_tasks: u32,
        at: i64,
    },
    ImageUpdated {
        env_id: String,
        status: String,
        message: String,
        at: i64,
    },
    Alert {
        rule: String,
        level: String,
        message: String,
        at: i64,
    },
    QueueDepth {
        queued: i64,
        running: i64,
        at: i64,
    },
}

impl Event {
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Event::TaskUpdated { task_id, .. } => Some(task_id),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        EventBus { tx }
    }

    pub fn publish(&self, event: Event) {
        // No subscribers is the normal case for a headless deployment.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        EventBus::new(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_event(id: &str, status: &str) -> Event {
        Event::TaskUpdated {
            task_id: id.into(),
            status: status.into(),
            result_kind: String::new(),
            task_type: "check".into(),
            project_id: "p".into(),
            worktree_id: "w".into(),
            worker_id: String::new(),
            at: 0,
        }
    }

    #[tokio::test]
    async fn subscribers_receive_events() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        bus.publish(task_event("t1", "running"));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.task_id(), Some("t1"));
    }

    #[tokio::test]
    async fn publishing_without_subscribers_is_not_an_error() {
        let bus = EventBus::new(8);
        bus.publish(task_event("t1", "queued"));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn a_slow_subscriber_lags_instead_of_blocking() {
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();
        for i in 0..10 {
            bus.publish(task_event(&format!("t{i}"), "queued"));
        }
        // The channel drops the oldest events; the producer never stalls.
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }

    #[test]
    fn events_serialize_with_a_discriminator_for_the_frontend() {
        let json = serde_json::to_string(&task_event("t1", "done")).unwrap();
        assert!(json.contains("\"type\":\"task_updated\""));
        assert!(json.contains("\"task_id\":\"t1\""));
    }
}
