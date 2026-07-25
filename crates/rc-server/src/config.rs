//! Server configuration.
//!
//! Everything has a working default so `rc-server serve` runs with no flags.
//! Operational knobs that an admin should be able to change without a restart
//! live in the `settings` table instead and are read through [`Policy`].

use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub http_addr: String,
    pub grpc_addr: String,
    /// Skip agent-token checks. Only for single-user local experiments.
    pub allow_anonymous_agents: bool,
    pub session_ttl_secs: i64,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("rc-server.sqlite")
    }
    pub fn cas_path(&self) -> PathBuf {
        self.data_dir.join("cas")
    }
}

/// Runtime-tunable policy, persisted in `settings` and editable from the
/// Settings page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    /// §5.1 — non-deterministic builds make an unbounded task cache unsafe.
    pub task_cache_ttl_secs: i64,
    /// §5.3 — the only thing that reaps abandoned queue entries.
    pub pending_ttl_secs: i64,
    /// §9 — CAS blobs with no reference and no recent use.
    pub blob_gc_ttl_secs: i64,
    /// §9 — full build logs.
    pub log_retention_secs: i64,
    /// §9 — worker considered gone; its exclusive caches become reclaimable.
    pub worker_offline_secs: i64,
    /// §6.2 — infra failures retried on a *different* worker this many times.
    pub max_infra_retries: i64,
    /// §8.3 — new image digests need an admin before they run untrusted code.
    pub require_image_approval: bool,
    /// §11 — structured diagnostics returned inline to the agent.
    pub max_diagnostics: usize,
    /// §6.1 scoring weights.
    pub w_disk: f64,
    pub w_cpu: f64,
    pub w_cache_affinity: f64,
    pub w_image_affinity: f64,
    /// Hard filter: worker is skipped below this much free disk.
    pub min_disk_free_gb: u64,
    /// §15.3 — alert webhook (DingTalk / Feishu / Slack compatible).
    pub alert_webhook: String,
    /// Fallback image when nothing else is known.
    pub default_image: String,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            task_cache_ttl_secs: rc_core::TASK_CACHE_TTL_SECS,
            pending_ttl_secs: rc_core::PENDING_TTL_SECS,
            blob_gc_ttl_secs: 30 * 24 * 3600,
            log_retention_secs: 7 * 24 * 3600,
            worker_offline_secs: 24 * 3600,
            max_infra_retries: 2,
            require_image_approval: true,
            max_diagnostics: 10,
            w_disk: 1.0,
            w_cpu: 1.0,
            w_cache_affinity: 2.0,
            w_image_affinity: 0.8,
            min_disk_free_gb: 20,
            alert_webhook: String::new(),
            default_image: "docker.io/library/rust:1-bookworm".into(),
        }
    }
}

const POLICY_KEY: &str = "policy";

impl Policy {
    pub fn load(store: &Store) -> Self {
        match store.get_setting(POLICY_KEY) {
            Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "stored policy is unreadable, using defaults");
                Policy::default()
            }),
            _ => Policy::default(),
        }
    }

    pub fn save(&self, store: &Store) -> anyhow::Result<()> {
        store.set_setting(POLICY_KEY, &serde_json::to_string(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn policy_roundtrips_through_settings() {
        let s = Store::open_memory().unwrap();
        let mut p = Policy::default();
        p.max_infra_retries = 5;
        p.alert_webhook = "https://example.invalid/hook".into();
        p.save(&s).unwrap();
        let loaded = Policy::load(&s);
        assert_eq!(loaded.max_infra_retries, 5);
        assert_eq!(loaded.alert_webhook, "https://example.invalid/hook");
    }

    #[test]
    fn corrupt_policy_falls_back_to_defaults() {
        let s = Store::open_memory().unwrap();
        s.set_setting(POLICY_KEY, "{not json").unwrap();
        assert_eq!(Policy::load(&s).max_infra_retries, Policy::default().max_infra_retries);
    }

    #[test]
    fn approval_is_required_by_default() {
        // §8.3: image build is its own attack surface; opt-out must be explicit.
        assert!(Policy::default().require_image_approval);
    }
}
