//! Built-in time series (§15.1).
//!
//! Two audiences with different needs, one collection point:
//!   * the built-in dashboard, which must work with zero external
//!     dependencies — samples accumulate in memory and are flushed to SQLite
//!     as batched rollups (1min kept 7 days, 1h kept 90 days);
//!   * an existing Prometheus stack, which scrapes `/metrics`.
//!
//! Nothing here writes to SQLite per sample: the store has a single writer and
//! second-granularity writes would show up as API latency (risk #28).

use crate::store::Store;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Counter,
    Gauge,
}

#[derive(Default)]
struct Inner {
    counters: BTreeMap<String, f64>,
    gauges: BTreeMap<String, f64>,
    /// Samples not yet folded into a rollup bucket.
    pending: BTreeMap<(String, i64), (f64, i64)>,
    help: BTreeMap<String, (Kind, String)>,
}

#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Mutex<Inner>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let m = Metrics {
            inner: Arc::new(Mutex::new(Inner::default())),
        };
        for (name, kind, help) in DECLARED {
            m.declare(name, *kind, help);
        }
        m
    }

    fn declare(&self, name: &str, kind: Kind, help: &str) {
        let mut g = self.inner.lock();
        g.help.insert(name.to_string(), (kind, help.to_string()));
        match kind {
            Kind::Counter => {
                g.counters.entry(name.to_string()).or_insert(0.0);
            }
            Kind::Gauge => {
                g.gauges.entry(name.to_string()).or_insert(0.0);
            }
        }
    }

    /// Monotonic counter, also folded into the 1-minute rollup bucket.
    pub fn incr(&self, name: &str, delta: f64) {
        let bucket = bucket_of(rc_core::now_secs(), 60);
        let mut g = self.inner.lock();
        *g.counters.entry(name.to_string()).or_insert(0.0) += delta;
        let e = g.pending.entry((name.to_string(), bucket)).or_insert((0.0, 0));
        e.0 += delta;
        e.1 += 1;
    }

    pub fn set(&self, name: &str, value: f64) {
        let bucket = bucket_of(rc_core::now_secs(), 60);
        let mut g = self.inner.lock();
        g.gauges.insert(name.to_string(), value);
        let e = g.pending.entry((name.to_string(), bucket)).or_insert((0.0, 0));
        e.0 += value;
        e.1 += 1;
    }

    /// Record a duration/size observation; the rollup stores sum and count so
    /// the dashboard can render an average per bucket.
    pub fn observe(&self, name: &str, value: f64) {
        let bucket = bucket_of(rc_core::now_secs(), 60);
        let mut g = self.inner.lock();
        let e = g.pending.entry((name.to_string(), bucket)).or_insert((0.0, 0));
        e.0 += value;
        e.1 += 1;
        *g.counters.entry(format!("{name}_sum")).or_insert(0.0) += value;
        *g.counters.entry(format!("{name}_count")).or_insert(0.0) += 1.0;
    }

    /// Move everything buffered in memory into SQLite in one transaction.
    /// Buckets from the current minute are kept back so they are not written
    /// half-complete.
    pub fn flush(&self, store: &Store) -> anyhow::Result<usize> {
        let current = bucket_of(rc_core::now_secs(), 60);
        let drained: Vec<(String, i64, f64, i64)> = {
            let mut g = self.inner.lock();
            let ready: Vec<(String, i64)> = g
                .pending
                .keys()
                .filter(|(_, b)| *b < current)
                .cloned()
                .collect();
            ready
                .into_iter()
                .filter_map(|k| g.pending.remove(&k).map(|(sum, count)| (k.0, k.1, sum, count)))
                .collect()
        };
        if drained.is_empty() {
            return Ok(0);
        }
        store.write_rollup("1min", &drained)?;
        // Fold the same points into hourly buckets for the long window.
        let hourly: Vec<(String, i64, f64, i64)> = drained
            .iter()
            .map(|(m, b, s, c)| (m.clone(), bucket_of(*b, 3600), *s, *c))
            .collect();
        store.write_rollup("1hour", &hourly)?;
        Ok(drained.len())
    }

    /// Prometheus text exposition (§15.1 layer 2).
    pub fn render_prometheus(&self) -> String {
        let g = self.inner.lock();
        let mut out = String::new();
        for (name, value) in g.counters.iter() {
            let full = format!("rc_{name}");
            if let Some((kind, help)) = g.help.get(name) {
                out.push_str(&format!("# HELP {full} {help}\n"));
                out.push_str(&format!(
                    "# TYPE {full} {}\n",
                    if *kind == Kind::Counter { "counter" } else { "gauge" }
                ));
            }
            out.push_str(&format!("{full} {value}\n"));
        }
        for (name, value) in g.gauges.iter() {
            let full = format!("rc_{name}");
            if let Some((_, help)) = g.help.get(name) {
                out.push_str(&format!("# HELP {full} {help}\n"));
                out.push_str(&format!("# TYPE {full} gauge\n"));
            }
            out.push_str(&format!("{full} {value}\n"));
        }
        out
    }

    pub fn snapshot(&self) -> (BTreeMap<String, f64>, BTreeMap<String, f64>) {
        let g = self.inner.lock();
        (g.counters.clone(), g.gauges.clone())
    }
}

pub fn bucket_of(ts_secs: i64, granularity: i64) -> i64 {
    ts_secs - ts_secs.rem_euclid(granularity)
}

/// Metric catalogue (§15.2). Declaring them up front means `/metrics` shows a
/// zero rather than nothing at all before the first event.
pub const DECLARED: &[(&str, Kind, &str)] = &[
    ("tasks_submitted_total", Kind::Counter, "Tasks accepted from agents"),
    ("tasks_completed_total", Kind::Counter, "Tasks that reached a terminal state"),
    ("tasks_success_total", Kind::Counter, "Tasks whose result was success"),
    ("tasks_compile_error_total", Kind::Counter, "Tasks that failed to compile"),
    ("tasks_env_error_total", Kind::Counter, "Tasks that failed on the environment"),
    ("tasks_infra_error_total", Kind::Counter, "Tasks that failed on infrastructure"),
    ("tasks_timeout_total", Kind::Counter, "Tasks killed by the hard timeout"),
    ("tasks_superseded_total", Kind::Counter, "Tasks cancelled by a newer submission"),
    ("tasks_cache_hit_total", Kind::Counter, "Submissions served from the task cache"),
    ("tasks_dedup_total", Kind::Counter, "Submissions attached to an in-flight identical task"),
    ("tasks_retried_total", Kind::Counter, "Infra-error retries dispatched to another worker"),
    ("blobs_uploaded_total", Kind::Counter, "Blobs accepted into the CAS"),
    ("blobs_reconciled_total", Kind::Counter, "Blob hashes checked during reconciliation"),
    ("blobs_missing_total", Kind::Counter, "Blob hashes the agent had to upload"),
    ("blob_bytes_uploaded_total", Kind::Counter, "Bytes written into the CAS"),
    ("blob_missing_selfheal_total", Kind::Counter, "Tasks recovered from a blob_missing report"),
    ("gc_blobs_deleted_total", Kind::Counter, "CAS blobs reclaimed by GC"),
    ("gc_bytes_reclaimed_total", Kind::Counter, "Bytes reclaimed by GC"),
    ("images_built_total", Kind::Counter, "Environment image builds finished"),
    ("api_requests_total", Kind::Counter, "Admin API requests served"),
    ("queue_depth", Kind::Gauge, "Tasks waiting for a worker"),
    ("running_tasks", Kind::Gauge, "Tasks currently executing"),
    ("workers_online", Kind::Gauge, "Workers with a live channel"),
    ("sse_connections", Kind::Gauge, "Open admin SSE streams"),
    ("cas_bytes", Kind::Gauge, "Total CAS size in bytes"),
    ("cas_blobs", Kind::Gauge, "Total CAS blob count"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let m = Metrics::new();
        m.incr("tasks_submitted_total", 1.0);
        m.incr("tasks_submitted_total", 2.0);
        assert_eq!(m.snapshot().0["tasks_submitted_total"], 3.0);
    }

    #[test]
    fn gauges_replace_rather_than_add() {
        let m = Metrics::new();
        m.set("queue_depth", 5.0);
        m.set("queue_depth", 2.0);
        assert_eq!(m.snapshot().1["queue_depth"], 2.0);
    }

    #[test]
    fn prometheus_output_is_well_formed() {
        let m = Metrics::new();
        m.incr("tasks_success_total", 4.0);
        let text = m.render_prometheus();
        assert!(text.contains("# TYPE rc_tasks_success_total counter"));
        assert!(text.contains("rc_tasks_success_total 4"));
        assert!(text.contains("# TYPE rc_queue_depth gauge"));
    }

    #[test]
    fn declared_metrics_start_at_zero() {
        // A dashboard panel showing "no data" is indistinguishable from a
        // broken exporter; an explicit 0 is not.
        let text = Metrics::new().render_prometheus();
        assert!(text.contains("rc_tasks_submitted_total 0"));
    }

    #[test]
    fn flush_holds_back_the_in_progress_bucket() {
        let store = Store::open_memory().unwrap();
        let m = Metrics::new();
        m.incr("tasks_submitted_total", 1.0);
        // The only pending bucket is the current minute, so nothing is ready.
        assert_eq!(m.flush(&store).unwrap(), 0);
    }

    #[test]
    fn flush_writes_completed_buckets_to_both_granularities() {
        let store = Store::open_memory().unwrap();
        let m = Metrics::new();
        {
            let mut g = m.inner.lock();
            g.pending.insert(("tasks_submitted_total".into(), 60), (7.0, 3));
        }
        assert_eq!(m.flush(&store).unwrap(), 1);
        assert_eq!(store.read_series("tasks_submitted_total", "1min", 0).unwrap(), vec![(60, 7.0, 3)]);
        assert_eq!(store.read_series("tasks_submitted_total", "1hour", 0).unwrap(), vec![(0, 7.0, 3)]);
    }

    #[test]
    fn buckets_align_to_the_granularity() {
        assert_eq!(bucket_of(125, 60), 120);
        assert_eq!(bucket_of(3599, 3600), 0);
        assert_eq!(bucket_of(3600, 3600), 3600);
    }

    #[test]
    fn observations_track_sum_and_count() {
        let m = Metrics::new();
        m.observe("build_ms", 100.0);
        m.observe("build_ms", 300.0);
        let (counters, _) = m.snapshot();
        assert_eq!(counters["build_ms_sum"], 400.0);
        assert_eq!(counters["build_ms_count"], 2.0);
    }
}
