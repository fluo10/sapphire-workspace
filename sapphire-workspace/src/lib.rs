pub mod config;
pub mod indexer;
pub mod workspace;
pub mod workspace_state;

mod error;
pub use error::{Error, Result};

pub use config::{EmbeddingConfig, SyncBackendKind, SyncConfig, UserConfig, VectorDb, WorkspaceConfig};
pub use workspace::DEFAULT_WORKSPACE_MARKER;
pub use indexer::path_to_doc_id;
pub use workspace::Workspace;
pub use workspace_state::{DbInfo, WorkspaceState};

// Re-export sapphire-retrieve public API so callers can use a single dependency.
pub use sapphire_retrieve::{
    Chunk, ChunkSearchResult, Document, Embedder, EmbeddingConfig as RetrieveEmbedConfig,
    Error as RetrieveError, RetrieveDb, SearchResult, VecInfo, build_embedder,
};
#[cfg(feature = "sqlite-store")]
pub use sapphire_retrieve::db::SCHEMA_VERSION as RETRIEVE_SCHEMA_VERSION;
#[cfg(feature = "lancedb-store")]
pub use sapphire_retrieve::lancedb_store;

// Re-export sapphire-sync public API.
pub use sapphire_sync::SyncBackend;
#[cfg(feature = "git-sync")]
pub use sapphire_sync::GitSync;
