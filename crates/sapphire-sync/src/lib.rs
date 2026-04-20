use std::path::Path;

pub mod config;
pub mod devices;
mod error;
pub use config::{SyncBackendKind, SyncConfig};
pub use devices::{DeviceContext, DeviceDefaults, DeviceRecord, DeviceRegistry, MergeOutcome};
pub use error::{Error, Result};

#[cfg(feature = "git")]
mod git_sync;
#[cfg(feature = "git")]
pub use git_sync::GitSync;

/// Abstraction over file-level sync backends (git, future P2P / rclone).
pub trait SyncBackend: Send + Sync {
    /// Stage `path` for the next sync operation (e.g. `git add`).
    fn add_file(&self, path: &Path) -> Result<()>;

    /// Remove `path` from the sync index (e.g. `git rm --cached`).
    fn remove_file(&self, path: &Path) -> Result<()>;

    /// Run the full sync cycle for this backend.
    ///
    /// For git: commit any staged changes, pull (merge) from remote, then push.
    /// Other backends may implement this differently (e.g. rclone sync).
    fn sync(&self) -> Result<()>;
}
