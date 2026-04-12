use serde::{Deserialize, Serialize};

use crate::embed::EmbedderConfig;

/// Top-level retrieve configuration (`[retrieve]` section).
///
/// Controls which vector database backend to use and, optionally, text
/// embedding settings for approximate semantic search.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetrieveConfig {
    /// Vector database backend (default: `none` — vector search disabled).
    #[serde(default)]
    pub db: VectorDb,
    /// Text embedding settings.  When absent, embedding is disabled even
    /// if `db` is set to a non-`none` value.
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,
    /// Hybrid search tuning (FTS + semantic merged via Reciprocal Rank Fusion).
    #[serde(default)]
    pub hybrid: HybridConfig,
    /// How often to automatically refresh the embedding cache, in minutes.
    ///
    /// When set, the `watch` command runs a mtime-based incremental cache
    /// update at this interval.
    /// When unset or `0`, automatic periodic cache refresh is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_interval_minutes: Option<u32>,
}

impl RetrieveConfig {
    /// Returns the cache refresh interval as a [`std::time::Duration`], or
    /// `None` if periodic refresh is disabled (`sync_interval_minutes` is
    /// unset or `0`).
    pub fn sync_interval(&self) -> Option<std::time::Duration> {
        self.sync_interval_minutes
            .filter(|&m| m > 0)
            .map(|m| std::time::Duration::from_secs(m as u64 * 60))
    }
}

/// Settings for hybrid (FTS + semantic) search merging via Reciprocal Rank
/// Fusion (RRF).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridConfig {
    /// Weight for FTS results in RRF fusion (0.0–1.0, default 0.5).
    /// The semantic weight is `1.0 - fts_weight`.
    #[serde(default = "default_fts_weight")]
    pub fts_weight: f64,
    /// Constant *k* in the RRF formula: `score = 1 / (k + rank)`.
    /// Default 60.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: u32,
}

fn default_fts_weight() -> f64 {
    0.5
}

fn default_rrf_k() -> u32 {
    60
}

impl Default for HybridConfig {
    fn default() -> Self {
        Self {
            fts_weight: default_fts_weight(),
            rrf_k: default_rrf_k(),
        }
    }
}

/// Vector database backend for approximate (semantic) text search.
///
/// | Variant      | Description                                              |
/// |--------------|----------------------------------------------------------|
/// | `none`       | Vector search disabled (default, no extra dependencies)  |
/// | `sqlite_vec` | sqlite-vec extension, stored inside the SQLite cache DB  |
/// | `lancedb`    | LanceDB — suitable for larger-scale / multimodal use     |
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VectorDb {
    /// Vector search is disabled. No embedding model is required.
    #[default]
    None,
    /// sqlite-vec extension stored in the existing SQLite cache database.
    SqliteVec,
    /// LanceDB stored in a separate data directory alongside the cache.
    #[serde(rename = "lancedb")]
    LanceDb,
}

impl VectorDb {
    /// Human-readable name, matching the TOML serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            VectorDb::None => "none",
            VectorDb::SqliteVec => "sqlite_vec",
            VectorDb::LanceDb => "lancedb",
        }
    }
}

/// Text embedding provider configuration (`[retrieve.embedding]` subsection).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmbeddingConfig {
    /// Enable embedding and vector search.
    #[serde(default)]
    pub enabled: bool,

    /// Embedding provider identifier: `"openai"`, `"ollama"`, or `"fastembed"`.
    #[serde(default)]
    pub provider: String,

    /// Model name understood by the provider.
    #[serde(default)]
    pub model: String,

    /// Name of the environment variable holding the API key.
    /// Used by OpenAI-compatible providers; defaults to `OPENAI_API_KEY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,

    /// Base URL of the embedding API endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Output vector dimension of the model.
    /// Required when `db = "sqlite_vec"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<u32>,
}

impl EmbeddingConfig {
    /// Convert to the runtime [`EmbedderConfig`] used by [`crate::build_embedder`].
    /// Convert to the runtime [`EmbedderConfig`].
    ///
    /// `cache_dir` is left as `None`; callers should set it to the
    /// app-provided model cache directory before calling [`crate::build_embedder`].
    pub fn to_embedder_config(&self) -> EmbedderConfig {
        EmbedderConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_key_env: self.api_key_env.clone(),
            base_url: self.base_url.clone(),
            cache_dir: None,
        }
    }
}
