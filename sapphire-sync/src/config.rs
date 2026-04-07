use serde::{Deserialize, Serialize};

/// Sync backend selection and options (`[sync]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncConfig {
    /// Which sync backend to use (default: `auto`).
    #[serde(default)]
    pub backend: SyncBackendKind,
    /// Remote name used by the git backend (default: `"origin"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Branch name (default: current HEAD branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// How often to automatically sync, in minutes.
    ///
    /// When set, the `watch` command will run a full sync cycle at this
    /// interval in addition to index updates triggered by file events.
    /// When unset or `0`, automatic periodic sync is disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_interval_minutes: Option<u32>,
}

impl SyncConfig {
    /// Effective remote name (falls back to `"origin"`).
    pub fn remote(&self) -> &str {
        self.remote.as_deref().unwrap_or("origin")
    }

    /// Returns the sync interval as a [`std::time::Duration`], or `None` if
    /// periodic sync is disabled (`sync_interval_minutes` is unset or `0`).
    pub fn sync_interval(&self) -> Option<std::time::Duration> {
        self.sync_interval_minutes
            .filter(|&m| m > 0)
            .map(|m| std::time::Duration::from_secs(m as u64 * 60))
    }
}

/// Supported sync backend variants.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncBackendKind {
    /// Auto-detect: use git if the workspace is inside a git repository,
    /// otherwise no sync (local-only).  This is the default.
    #[default]
    Auto,
    /// Explicitly disable sync — local-only even inside a git repository.
    None,
    /// Git-based sync (commit → pull → push).
    Git,
}
