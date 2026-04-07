use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ── WorkspaceConfig (per-workspace, stored in {marker}/config.toml) ──────────

/// All settings for a workspace.  Stored in `.sapphire-workspace/config.toml`
/// (or whichever marker directory the workspace uses).
///
/// This is the primary config for `sapphire-workspace`.  The legacy [`UserConfig`]
/// (XDG path) is kept for backward compatibility when no marker directory exists.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,
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

    /// Convert to [`UserConfig`] for use with [`WorkspaceState`] methods that
    /// still accept the legacy type.
    pub fn to_user_config(&self) -> UserConfig {
        UserConfig {
            embedding: self.embedding.clone(),
        }
    }
}

/// Sync backend selection and options (`[sync]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncConfig {
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
    /// Git-based sync (commit → pull → push via `sapphire-workspace sync`).
    Git,
}

// ── UserConfig (legacy, XDG path, backward compat) ───────────────────────────

/// Legacy per-user config loaded from `$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`.
///
/// Used as a fallback when no `.sapphire-workspace` marker directory is present.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,
}

impl UserConfig {
    /// Canonical path: `$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`.
    pub fn path() -> PathBuf {
        xdg_config_home()
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
        let enabled = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_ENABLED")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        let vector_db = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_VECTOR_DB")
            .ok()
            .and_then(|v| match v.as_str() {
                "none" => Some(VectorDb::None),
                "sqlite_vec" => Some(VectorDb::SqliteVec),
                "lancedb" => Some(VectorDb::LanceDb),
                _ => None,
            });
        let provider = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_PROVIDER").ok();
        let model = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_MODEL").ok();
        let api_key_env = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_API_KEY_ENV").ok();
        let base_url = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_BASE_URL").ok();
        let dimension = std::env::var("SAPPHIRE_WORKSPACE_EMBEDDING_DIMENSION")
            .ok()
            .and_then(|v| v.parse::<u32>().ok());

        let any = enabled.is_some()
            || vector_db.is_some()
            || provider.is_some()
            || model.is_some()
            || api_key_env.is_some()
            || base_url.is_some()
            || dimension.is_some();

        if any {
            let embed = self.embedding.get_or_insert_with(|| EmbeddingConfig {
                enabled: false,
                vector_db: VectorDb::default(),
                provider: String::new(),
                model: String::new(),
                api_key_env: None,
                base_url: None,
                dimension: None,
                extra: IndexMap::new(),
            });
            if let Some(v) = enabled { embed.enabled = v; }
            if let Some(v) = vector_db { embed.vector_db = v; }
            if let Some(v) = provider { embed.provider = v; }
            if let Some(v) = model { embed.model = v; }
            if let Some(v) = api_key_env { embed.api_key_env = Some(v); }
            if let Some(v) = base_url { embed.base_url = Some(v); }
            if let Some(v) = dimension { embed.dimension = Some(v); }
        }
    }
}

// ── Shared types ──────────────────────────────────────────────────────────────

/// Vector database backend for approximate (semantic) search.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VectorDb {
    #[default]
    None,
    SqliteVec,
    #[serde(rename = "lancedb")]
    LanceDb,
}

impl VectorDb {
    pub fn as_str(self) -> &'static str {
        match self {
            VectorDb::None => "none",
            VectorDb::SqliteVec => "sqlite_vec",
            VectorDb::LanceDb => "lancedb",
        }
    }
}

/// Text embedding provider configuration (`[embedding]` section).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub vector_db: VectorDb,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<u32>,
    #[serde(flatten)]
    pub extra: IndexMap<String, toml::Value>,
}

impl EmbeddingConfig {
    pub fn to_retrieve_embed_config(&self) -> sapphire_retrieve::EmbeddingConfig {
        sapphire_retrieve::EmbeddingConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_key_env: self.api_key_env.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

fn xdg_config_home() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir)
}
