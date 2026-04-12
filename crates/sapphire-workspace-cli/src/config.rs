//! User config for `sapphire-workspace-cli`.
//!
//! All settings are read from a single user-level file
//! (`$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`).
//! There is no workspace-level config layer — every setting is per-host.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sapphire_workspace::{EmbeddingConfig, RetrieveConfig, SyncConfig, VectorDb};
use serde::{Deserialize, Serialize};

use crate::WORKSPACE_CTX;

// ── UserConfig ────────────────────────────────────────────────────────────────

/// Per-user (per-host) configuration.
///
/// Stored at `$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`.
/// All settings here are host-specific: the embedding model depends on
/// local hardware, and `sync.device_id` must be unique per device.
///
/// TOML structure:
///
/// ```toml
/// [sync]
/// backend = "git"
/// remote = "origin"
/// sync_interval_minutes = 15
/// device_id = "..."
///
/// [retrieve]
/// db = "sqlite_vec"
/// sync_interval_minutes = 30
///
/// [retrieve.embedding]
/// enabled = true
/// provider = "fastembed"
/// model = "..."
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
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
            let contents =
                std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
            toml::from_str(&contents)
                .with_context(|| format!("failed to parse config at {}", path.display()))?
        };
        config.apply_env_overrides();
        Ok(config)
    }

    /// Serialize and write to `path` (creates parent directories).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self).context("failed to serialize config")?;
        std::fs::write(path, contents)
            .with_context(|| format!("failed to write config to {}", path.display()))?;
        Ok(())
    }

    /// Serialize and write to the canonical [`UserConfig::path`].
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
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

/// Ensure `sync.device_id` is present in the user config.
///
/// If absent, a random UUID v4 is generated and written back to the user
/// config file. Errors are propagated so the caller can decide whether to
/// abort or continue without a device ID.
pub fn ensure_device_id() -> Result<()> {
    let mut config = UserConfig::load().context("failed to load user config for device_id")?;
    if config.sync.ensure_device_id() {
        config
            .save()
            .context("failed to write device_id to user config")?;
    }
    Ok(())
}
