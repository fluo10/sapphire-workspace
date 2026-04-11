pub mod chunker;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
#[cfg(feature = "lancedb-store")]
pub mod lancedb_store;
pub mod retrieve_store;
#[cfg(feature = "sqlite-store")]
pub mod sqlite_store;
pub mod vector_store;

pub use chunker::{Chunker, JsonChunker, MarkdownChunker, TextChunk};
pub use config::{EmbeddingConfig, HybridConfig, RetrieveConfig, VectorDb};
pub use db::open_in_memory;
#[cfg(feature = "lancedb-store")]
pub use db::open_lancedb;
pub use db::{Document, RetrieveDb, SearchResult, dedup_chunk_results};
#[cfg(feature = "sqlite-store")]
pub use db::{open_sqlite_fts, open_sqlite_vec};
pub use embed::{Embedder, EmbedderConfig, build_embedder};
pub use error::{Error, Result};
pub use retrieve_store::RetrieveStore;
pub use vector_store::{Chunk, ChunkSearchResult, VecInfo, VectorStore};
