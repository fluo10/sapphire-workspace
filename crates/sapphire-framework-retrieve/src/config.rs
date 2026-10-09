use serde::{Deserialize, Serialize};

/// Top-level retrieve configuration (`[retrieve]` section).
///
/// Controls which vector database backend to use and how hybrid search is
/// merged. The embedding provider is configured by the bridge, not here.
///
/// Unknown keys are ignored (no `deny_unknown_fields`), so an older config
/// that still has a `[retrieve.embedding]` table keeps loading.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetrieveConfig {
    /// Vector database backend (default: `none` — vector search disabled).
    #[serde(default)]
    pub db: VectorDb,
    /// Hybrid search tuning (FTS + semantic merged via Reciprocal Rank Fusion).
    #[serde(default)]
    pub hybrid: HybridConfig,
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
/// | `redb`       | Brute-force vectors in the pure-Rust redb cache (default backend) |
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VectorDb {
    /// Vector search is disabled. No embedding model is required.
    #[default]
    None,
    /// Brute-force vector search stored in the pure-Rust redb cache.
    Redb,
}

impl VectorDb {
    /// Human-readable name, matching the TOML serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            VectorDb::None => "none",
            VectorDb::Redb => "redb",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Root {
        retrieve: RetrieveConfig,
    }

    #[test]
    fn an_old_config_with_an_embedding_table_still_loads() {
        let toml = r#"
[retrieve]
db = "redb"

[retrieve.embedding]
enabled = true
provider = "fastembed"
model = "all-MiniLM-L6-v2"
dimension = 384

[retrieve.hybrid]
rrf_k = 30
"#;
        let root: Root = toml::from_str(toml).unwrap();
        assert_eq!(root.retrieve.db, VectorDb::Redb);
        assert_eq!(root.retrieve.hybrid.rrf_k, 30);
    }
}
