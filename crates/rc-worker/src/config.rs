//! Worker configuration, persisted next to its data so a restart keeps its
//! identity and token.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub server: String,
    pub worker_id: String,
    pub worker_token: String,
    pub data_dir: PathBuf,
    pub max_parallel: u32,
    /// Hosts the egress proxy will talk to (§7.1). Suffix wildcards allowed.
    pub allowlist: Vec<String>,
    /// Memory cap per build container, in MB.
    pub memory_mb: u64,
    /// CPU cap per build container.
    pub cpus: f64,
    /// Fork-bomb guard.
    pub pids_limit: i64,
    /// Bytes a single egress tunnel may move before it is cut. Bandwidth is
    /// the only lever left against exfiltration through an allowed host
    /// (§7.1/§16).
    pub egress_byte_cap: u64,
    /// Give up on a worktree's caches after this long without a task (§9).
    pub worktree_idle_days: i64,
    /// Refuse new work below this much free disk.
    pub min_disk_free_gb: u64,
    pub labels: std::collections::BTreeMap<String, String>,
}

pub const DEFAULT_ALLOWLIST: &[&str] = &[
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "proxy.golang.org",
    "sh.rustup.rs",
    "static.rust-lang.org",
];

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            server: "http://127.0.0.1:7701".into(),
            worker_id: String::new(),
            worker_token: String::new(),
            data_dir: PathBuf::from("./rc-worker-data"),
            max_parallel: default_parallelism(),
            allowlist: DEFAULT_ALLOWLIST.iter().map(|s| s.to_string()).collect(),
            memory_mb: 8192,
            cpus: 4.0,
            pids_limit: 2048,
            egress_byte_cap: 2 * 1024 * 1024 * 1024,
            worktree_idle_days: 14,
            min_disk_free_gb: 20,
            labels: Default::default(),
        }
    }
}

fn default_parallelism() -> u32 {
    std::thread::available_parallelism()
        .map(|n| (n.get() as u32 / 4).max(1))
        .unwrap_or(2)
}

impl WorkerConfig {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("worker.json")
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path_in(dir);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read {} — run `rc-worker enroll` first", path.display()))?;
        let mut cfg: WorkerConfig = serde_json::from_str(&text)?;
        cfg.data_dir = dir.to_path_buf();
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        let path = Self::path_in(&self.data_dir);
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        // The worker token is a fleet credential.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn cas_dir(&self) -> PathBuf {
        self.data_dir.join("cas")
    }
    pub fn work_dir(&self) -> PathBuf {
        self.data_dir.join("work")
    }
    pub fn mirror_dir(&self) -> PathBuf {
        self.data_dir.join("mirrors")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.data_dir.join("state")
    }
    pub fn sccache_dir(&self) -> PathBuf {
        self.data_dir.join("sccache")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            self.cas_dir(),
            self.work_dir(),
            self.mirror_dir(),
            self.state_dir(),
            self.sccache_dir(),
        ] {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }

    pub fn arch(&self) -> String {
        std::env::consts::ARCH.to_string()
    }
}

#[cfg(test)]
// Fixtures are built field-by-field on purpose: it keeps each test's
// deviation from the default obvious.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!("rc-wcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = WorkerConfig::default();
        cfg.data_dir = dir.clone();
        cfg.worker_id = "worker-1".into();
        cfg.worker_token = "secret".into();
        cfg.save().unwrap();

        let loaded = WorkerConfig::load(&dir).unwrap();
        assert_eq!(loaded.worker_id, "worker-1");
        assert_eq!(loaded.worker_token, "secret");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rc-wperm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = WorkerConfig::default();
        cfg.data_dir = dir.clone();
        cfg.save().unwrap();
        let mode = std::fs::metadata(WorkerConfig::path_in(&dir)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "worker token must not be readable by others");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_config_says_what_to_do() {
        let err = WorkerConfig::load(Path::new("/nonexistent/rc")).unwrap_err();
        assert!(err.to_string().contains("enroll"));
    }

    #[test]
    fn defaults_leave_room_for_parallel_builds() {
        let c = WorkerConfig::default();
        assert!(c.max_parallel >= 1);
        assert!(c.pids_limit > 0, "a fork bomb guard must be set");
        assert!(c.allowlist.contains(&"crates.io".to_string()));
    }
}
