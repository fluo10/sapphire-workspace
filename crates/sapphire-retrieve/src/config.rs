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
