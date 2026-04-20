use serde::{Deserialize, Serialize};

// ── SyncConfig ───────────────────────────────────────────────────────────────

/// Sync configuration (`[sync]` TOML section).
///
/// ```toml
/// [sync]
/// backend = "git"
/// remote = "origin"
/// branch = "main"
/// ```
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
}

impl SyncConfig {
    /// Effective remote name (falls back to `"origin"`).
    pub fn remote(&self) -> &str {
        self.remote.as_deref().unwrap_or("origin")
    }
}

// ── SyncBackendKind ──────────────────────────────────────────────────────────

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
