//! Async, GUI-facing backend over a workspace.
//!
//! GUIs (egui desktop, a future WASM frontend) should not call the synchronous
//! `sapphire_workspace::WorkspaceState` directly — its methods block and its
//! search takes a `&rusqlite::Connection`-style borrow that leaks storage
//! details. This crate hides that behind an `async` [`WorkspaceBackend`] with
//! two implementations:
//!
//! - [`LocalBackend`] wraps a local [`WorkspaceState`] and runs its blocking
//!   operations on the tokio blocking pool.
//! - [`IpcBackend`] forwards every call to the application's server, which is
//!   the only process that may open the cache.
//!
//! All emit [`BackendEvent`]s over a broadcast channel so a UI can refresh
//! reactively (see [`WorkspaceBackend::subscribe`]).
//!
//! The trait is `Send`/`Sync` and its futures are `Send`, which fits a native
//! egui app that holds a **concrete** backend and drives it with
//! `runtime.spawn` (the existing app.rs pattern). A `?Send` variant for WASM's
//! single-threaded executor is left to the frontend crate.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::sync::broadcast;

mod error;
mod ipc;
mod local;
mod source;

/// The methods every app server serves and their types, from
/// `sapphire-framework-server-api`, which carries the API version in its own version.
pub use sapphire_server_api as protocol;
pub use sapphire_server_api::{BackendEvent, FileSearchResult, SearchMode};

pub use error::{Error, Result};
pub use ipc::IpcBackend;
pub use local::LocalBackend;
pub use source::{
    DEFAULT_ID, WorkspaceEntry, WorkspaceLocator, WorkspaceRegistry, WorkspaceSelection,
    WorkspaceSource,
};

// Re-export the underlying state, which the factory needs, so callers depend only on
// this crate.
pub use sapphire_workspace::WorkspaceState;

/// Result of a sync cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncSummary {
    /// Number of documents added or updated.
    pub upserted: usize,
    /// Number of documents removed.
    pub removed: usize,
}

/// Async facade over a workspace's document operations, search and sync.
#[async_trait]
pub trait WorkspaceBackend: Send + Sync {
    /// Search the workspace, returning file-level results best-first.
    async fn search(
        &self,
        query: &str,
        limit: usize,
        mode: SearchMode,
    ) -> Result<Vec<FileSearchResult>>;

    /// Read a text file's full contents.
    async fn read_file(&self, path: &Path) -> Result<String>;

    /// Create or overwrite a text file, updating the index/sync backend.
    async fn write_file(&self, path: &Path, content: &str) -> Result<()>;

    /// Append to a text file, creating it if necessary.
    async fn append_file(&self, path: &Path, content: &str) -> Result<()>;

    /// Delete a file, updating the index/sync backend.
    async fn delete_file(&self, path: &Path) -> Result<()>;

    /// List the direct children of a directory as `(path, is_dir)` pairs.
    async fn list_dir(&self, path: &Path) -> Result<Vec<(PathBuf, bool)>>;

    /// Run a sync cycle and return what changed.
    async fn sync(&self) -> Result<SyncSummary>;

    /// Subscribe to [`BackendEvent`]s. Each subscriber gets its own receiver.
    fn subscribe(&self) -> broadcast::Receiver<BackendEvent>;
}
