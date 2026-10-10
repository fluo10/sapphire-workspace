pub mod app_dirs;
pub mod args;
pub mod bridge_embedder;
pub mod config;
pub mod context;
pub mod indexer;
pub mod logging;
pub mod vectors;
pub mod workspace;
pub mod workspace_state;

mod error;
#[cfg(test)]
mod test_env;

pub use app_dirs::AppKind;
pub use args::WorkspaceArgs;
pub use error::{Error, Result};

// Re-exported so apps can depend on this crate alone: `WorkspaceArgs` below is
// a clap `Args`, and `context` resolves platform directories through `dirs`
// (issues #128/#129). Apps that depend on this crate alone get the same
// clap/serde/dirs versions it builds with.
pub use clap;
pub use dirs;
pub use serde;

pub use bridge_embedder::BridgeEmbedder;
pub use config::{HybridConfig, RetrieveConfig, VectorDb};
pub use context::AppContext;
pub use indexer::{
    IndexHook, SyncReport, SyncWithHookError, path_to_doc_id, sync_workspace,
    sync_workspace_full_with_hook, sync_workspace_incremental, sync_workspace_with_hook,
};
pub use sapphire_bridge_api::EmbedModelInfo;
pub use workspace::Workspace;
pub use workspace::{DEFAULT_WORKSPACE_MARKER, path_uuid};
pub use workspace_state::{DbInfo, EmbedPolicy, RetrieveParams, SearchMode, WorkspaceState};

// Re-export sapphire-retrieve public API so callers can use a single dependency.
/// Retrieve cache schema version. Retained for API compatibility with the
/// retired SQLite backend; the pure-Rust redb backend manages its own on-disk
/// format, so this is always `0`.
pub const RETRIEVE_SCHEMA_VERSION: i32 = 0;
pub use sapphire_retrieve::{
    Document, Embedder, Error as RetrieveError, FileSearchResult, FtsQuery, HybridQuery,
    RetrieveDb, RetrieveStore, VecInfo, VectorQuery, default_hybrid, merge_rrf_files,
};
