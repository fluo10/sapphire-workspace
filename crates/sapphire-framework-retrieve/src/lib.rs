pub mod config;
pub mod db;
pub mod embed;
pub mod error;
#[cfg(feature = "redb-store")]
pub mod redb_store;
pub mod retrieve_store;
pub mod snippet;
pub mod vector_store;

pub use config::{EmbeddingConfig, HybridConfig, RetrieveConfig, VectorDb};
pub use db::open_in_memory;
pub use db::{RetrieveDb, default_hybrid, merge_rrf_files};
#[cfg(feature = "redb-store")]
pub use db::{open_redb, open_redb_vec};
pub use embed::{Embedder, EmbedderConfig, build_embedder};
pub use error::{Error, Result};
pub use retrieve_store::{
    Document, FileSearchResult, FtsQuery, HybridQuery, RetrieveStore, VectorQuery,
};
pub use snippet::SNIPPET_CHARS;
pub use vector_store::VecInfo;
