use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// Re-export config types from their home crates.
pub use sapphire_retrieve::config::{EmbeddingConfig, HybridConfig, RetrieveConfig, VectorDb};
pub use sapphire_sync::config::{SyncBackendKind, SyncConfig};

// ── WorkspaceConfig (per-workspace, stored in {marker}/config.toml) ──────────

/// All settings for a workspace.  Stored in `.sapphire-workspace/config.toml`
/// (or whichever marker directory the workspace uses).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub retrieve: RetrieveConfig,
}

impl WorkspaceConfig {
    /// Load from an explicit file path.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)?;
        toml::from_str(&contents)
            .map_err(|e| Error::ConfigParse { path: path.to_owned(), source: e })
    }

    /// Serialize and write to `path` (creates parent directories).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Convert to [`UserConfig`] for use with [`WorkspaceState`](crate::WorkspaceState) methods.
    pub fn to_user_config(&self) -> UserConfig {
        UserConfig {
            retrieve: Some(self.retrieve.clone()),
        }
    }
}

// ── UserConfig (legacy, XDG path, backward compat) ───────────────────────────

/// Legacy per-user config loaded from `$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`.
///
/// Used as a fallback when no `.sapphire-workspace` marker directory is present.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    #[serde(default)]
    pub retrieve: Option<RetrieveConfig>,
}

impl UserConfig {
    /// Canonical path: `$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`.
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("sapphire-workspace-cli")
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
            let contents = std::fs::read_to_string(&path)?;
            toml::from_str(&contents)
                .map_err(|e| Error::ConfigParse { path, source: e })?
        };
        config.apply_env_overrides();
        Ok(config)
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
            let retrieve = self.retrieve.get_or_insert_with(RetrieveConfig::default);
            if let Some(v) = db { retrieve.db = v; }
            let embed = retrieve.embedding.get_or_insert_with(EmbeddingConfig::default);
            if let Some(v) = enabled { embed.enabled = v; }
            if let Some(v) = provider { embed.provider = v; }
            if let Some(v) = model { embed.model = v; }
            if let Some(v) = api_key_env { embed.api_key_env = Some(v); }
            if let Some(v) = base_url { embed.base_url = Some(v); }
            if let Some(v) = dimension { embed.dimension = Some(v); }
        }
    }
}
