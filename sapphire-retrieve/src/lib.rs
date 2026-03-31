pub mod chunker;
pub mod db;
pub mod embed;
pub mod error;
#[cfg(feature = "lancedb-store")]
pub mod lancedb_store;
pub mod retrieve_store;
pub mod sqlite_store;
pub mod vector_store;

pub use db::{Document, RetrieveDb, SearchResult};
pub use embed::{build_embedder, EmbeddingConfig, Embedder};
pub use error::{Error, Result};
pub use retrieve_store::RetrieveStore;
pub use vector_store::{Chunk, ChunkSearchResult, VecInfo, VectorStore};
