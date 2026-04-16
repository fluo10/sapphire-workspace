pub mod config;
pub mod context;
pub mod indexer;
pub mod workspace;
pub mod workspace_state;

mod error;
pub use error::{Error, Result};

pub use config::{
    EmbeddingConfig, HybridConfig, RetrieveConfig, SyncBackendKind, SyncConfig, UserSyncConfig,
    VectorDb, WorkspaceSyncConfig,
};
pub use context::AppContext;
pub use indexer::path_to_doc_id;
pub use workspace::Workspace;
pub use workspace::{DEFAULT_WORKSPACE_MARKER, path_uuid};
pub use workspace_state::{DbInfo, RetrieveParams, SearchMode, WorkspaceState};

// Re-export sapphire-retrieve public API so callers can use a single dependency.
#[cfg(feature = "sqlite-store")]
pub use sapphire_retrieve::db::SCHEMA_VERSION as RETRIEVE_SCHEMA_VERSION;
#[cfg(feature = "lancedb-store")]
pub use sapphire_retrieve::lancedb_store;
pub use sapphire_retrieve::{
    Chunk, ChunkHit, Document, Embedder, EmbedderConfig, Error as RetrieveError, FileSearchResult,
    FtsQuery, HybridQuery, RetrieveDb, RetrieveStore, VecInfo, VectorQuery, build_embedder,
    default_hybrid, merge_rrf_files,
};

// Re-export sapphire-sync public API.
#[cfg(feature = "git-sync")]
pub use sapphire_sync::GitSync;
pub use sapphire_sync::SyncBackend;
