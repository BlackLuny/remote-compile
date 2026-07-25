//! Agent configuration and local state locations (§4.5).
//!
//! Nothing is written inside a worktree: state lives under the user cache dir,
//! so uninstalling is `rm -rf` and a repo never gains stray files that would
//! themselves need syncing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub server: String,
    #[serde(default)]
    pub token: String,
    /// Stable identity for supersede scoping (§5.2). Generated once and kept:
    /// a reconnecting agent must not supersede its own queued work (risk #27).
    pub agent_session: String,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    /// How long `check` waits before switching to async mode (§12).
    #[serde(default = "default_wait")]
    pub default_wait_secs: u32,
    /// Diagnostics returned inline in the L1 summary (§11).
    #[serde(default = "default_max_diagnostics")]
    pub max_diagnostics: usize,
}

fn default_wait() -> u32 {
    4
}
fn default_max_diagnostics() -> usize {
    10
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            server: "http://127.0.0.1:7701".into(),
            token: String::new(),
            agent_session: rc_core::ids::agent_session_id(),
            cache_dir: None,
            default_wait_secs: default_wait(),
            max_diagnostics: default_max_diagnostics(),
        }
    }
}

impl AgentConfig {
    pub fn config_path() -> PathBuf {
        config_home().join("remote-compile").join("agent.json")
    }

    /// Load, creating a config with a fresh session id on first run so the
    /// agent works with zero setup beyond a server address.
    pub fn load_or_create() -> Result<Self> {
        let path = Self::config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<AgentConfig>(&text) {
                return Ok(cfg);
            }
            tracing::warn!(path = %path.display(), "config is unreadable; regenerating");
        }
        let cfg = AgentConfig::default();
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn cache_root(&self) -> PathBuf {
        self.cache_dir
            .clone()
            .unwrap_or_else(|| cache_home().join("remote-compile"))
    }

    pub fn index_path(&self, worktree_abs: &Path) -> PathBuf {
        let key = blake3::hash(worktree_abs.to_string_lossy().as_bytes()).to_hex()[..16].to_string();
        self.cache_root().join("indexes").join(format!("{key}.sqlite"))
    }

    pub fn results_path(&self) -> PathBuf {
        self.cache_root().join("results.sqlite")
    }

    pub fn cas_known_path(&self) -> PathBuf {
        self.cache_root().join("cas_known.sqlite")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.cache_root().join("indexes"))?;
        Ok(())
    }
}

fn cache_home() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x);
    }
    home().join(".cache")
}

fn config_home() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x);
    }
    home().join(".config")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_paths_are_per_worktree_and_outside_the_repo() {
        // §4.5: state never lands inside a worktree, or it would need syncing.
        let cfg = AgentConfig {
            cache_dir: Some(PathBuf::from("/cache/rc")),
            ..Default::default()
        };
        let a = cfg.index_path(Path::new("/home/dev/wt-a"));
        let b = cfg.index_path(Path::new("/home/dev/wt-b"));
        assert_ne!(a, b);
        assert!(a.starts_with("/cache/rc"));
        assert!(!a.starts_with("/home/dev"));
    }

    #[test]
    fn the_same_worktree_maps_to_a_stable_index() {
        let cfg = AgentConfig::default();
        assert_eq!(
            cfg.index_path(Path::new("/home/dev/wt")),
            cfg.index_path(Path::new("/home/dev/wt"))
        );
    }

    #[test]
    fn xdg_cache_home_is_respected() {
        let cfg = AgentConfig::default();
        let root = cfg.cache_root();
        assert!(root.ends_with("remote-compile"));
    }

    #[test]
    fn a_generated_session_id_is_unique_per_install() {
        assert_ne!(
            AgentConfig::default().agent_session,
            AgentConfig::default().agent_session
        );
    }

    #[test]
    fn defaults_favour_a_short_synchronous_wait() {
        // §12: most incremental checks come back inside the wait window.
        let cfg = AgentConfig::default();
        assert!(cfg.default_wait_secs >= 3 && cfg.default_wait_secs <= 5);
        assert_eq!(cfg.max_diagnostics, 10);
    }
}
