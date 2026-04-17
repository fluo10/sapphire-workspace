use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Path could not be accessed (canonicalize / stat failed).
    #[error("cannot access '{path}': {source}")]
    Access {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Marker directory not found while walking up from `start`.
    #[error("no '{marker}' directory found in '{start}' or any parent")]
    MarkerNotFound { marker: String, start: PathBuf },

    /// Workspace opened by root path but the marker directory is missing.
    #[error("workspace marker '{marker}' not found in '{root}'")]
    MarkerDirMissing { marker: String, root: PathBuf },

    /// Config file could not be parsed.
    #[error("invalid config at '{path}': {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// Config serialization failed.
    #[error("failed to serialize workspace config: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    /// LanceDB feature not compiled in.
    #[error("lancedb support is not compiled in (enable the `lancedb-store` feature)")]
    LanceDbNotEnabled,

    /// SQLite store feature not compiled in.
    #[error("sqlite-store support is not compiled in (enable the `sqlite-store` feature)")]
    SqliteStoreNotEnabled,

    /// A path resolved to a location outside the workspace root and
    /// [`AppContext::allows_external_paths`](crate::AppContext::allows_external_paths)
    /// is `false`.
    #[error("path '{path}' escapes workspace root '{root}'")]
    PathEscapesWorkspace { path: PathBuf, root: PathBuf },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Retrieve(#[from] sapphire_retrieve::Error),

    #[error(transparent)]
    Sync(#[from] sapphire_sync::Error),

    #[error(transparent)]
    Walkdir(#[from] walkdir::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
