//! User config for `sapphire-workspace-cli`.
//!
//! Settings are read from a single user-level file
//! (`$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`).
//! There is no workspace-level config layer — every setting is per-host.
//!
//! The auto-generated device id lives in [`AppContext::device_id`], stored
//! at `<data_dir>/device_id`, so the CLI never has to rewrite the
//! user-edited `config.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use sapphire_workspace::{EmbeddingConfig, RetrieveConfig, SyncConfig, VectorDb};
use serde::{Deserialize, Serialize};

use crate::WORKSPACE_CTX;

// ── UserConfig ────────────────────────────────────────────────────────────────

/// Per-user (per-host) configuration.
///
/// Stored at `$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`.
/// All settings here are host-specific (e.g. the embedding model depends on
/// local hardware).
///
/// TOML structure:
///
/// ```toml
/// sync_interval_minutes = 15
///
/// [sync]
/// backend = "git"
/// remote = "origin"
///
/// [retrieve]
/// db = "sqlite_vec"
///
/// [retrieve.embedding]
/// enabled = true
/// provider = "fastembed"
/// model = "..."
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    /// How often the `watch` command runs the periodic sync cycle
    /// (git sync + retrieve cache refresh), in minutes.
    ///
    /// Unset or `0` disables periodic sync entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_interval_minutes: Option<u32>,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub retrieve: RetrieveConfig,
}

impl UserConfig {
    /// Canonical path: `$XDG_CONFIG_HOME/{app_name}/config.toml`.
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(WORKSPACE_CTX.app_name)
            .join("config.toml")
    }

    /// Load config from disk, then apply environment variable overrides.
    ///
    /// Returns the default config if the file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::path();
        let mut config = if !path.exists() {
            UserConfig::default()
        } else {
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            toml::from_str(&contents)
                .with_context(|| format!("failed to parse config at {}", path.display()))?
        };
        config.apply_env_overrides();
        Ok(config)
    }

    /// Returns the periodic sync interval as a [`std::time::Duration`], or
    /// `None` if periodic sync is disabled (`sync_interval_minutes` is unset
    /// or `0`).
    pub fn sync_interval(&self) -> Option<std::time::Duration> {
        self.sync_interval_minutes
            .filter(|&m| m > 0)
            .map(|m| std::time::Duration::from_secs(m as u64 * 60))
    }

    fn apply_env_overrides(&mut self) {
        let db = std::env::var("SAPPHIRE_WORKSPACE_RETRIEVE_DB")
            .ok()
            .and_then(|v| match v.as_str() {
                "none" => Some(VectorDb::None),
                "sqlite_vec" => Some(VectorDb::SqliteVec),
                "lancedb" => Some(VectorDb::LanceDb),
                _ => None,
            });
        let enabled = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_ENABLED")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        let provider = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_PROVIDER").ok();
        let model = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_MODEL").ok();
        let api_key_env = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_API_KEY_ENV").ok();
        let base_url = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_BASE_URL").ok();
        let dimension = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_DIMENSION")
            .ok()
            .and_then(|v| v.parse::<u32>().ok());

        let any = db.is_some()
            || enabled.is_some()
            || provider.is_some()
            || model.is_some()
            || api_key_env.is_some()
            || base_url.is_some()
            || dimension.is_some();

        if any {
            let retrieve = &mut self.retrieve;
            if let Some(v) = db {
                retrieve.db = v;
            }
            let embed = retrieve
                .embedding
                .get_or_insert_with(EmbeddingConfig::default);
            if let Some(v) = enabled {
                embed.enabled = v;
            }
            if let Some(v) = provider {
                embed.provider = v;
            }
            if let Some(v) = model {
                embed.model = v;
            }
            if let Some(v) = api_key_env {
                embed.api_key_env = Some(v);
            }
            if let Some(v) = base_url {
                embed.base_url = Some(v);
            }
            if let Some(v) = dimension {
                embed.dimension = Some(v);
            }
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Load the user config from disk (with environment variable overrides applied).
///
/// Returns the default `UserConfig` if the file does not exist.
pub fn load_user_config() -> Result<UserConfig> {
    UserConfig::load()
}
