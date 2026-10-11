use thiserror::Error;

/// Errors surfaced by a [`WorkspaceBackend`](crate::WorkspaceBackend).
#[derive(Debug, Error)]
pub enum Error {
    /// The underlying local workspace failed.
    #[error(transparent)]
    Workspace(#[from] sapphire_workspace::Error),

    /// The IPC layer failed.
    #[error(transparent)]
    Ipc(#[from] sapphire_ipc::Error),

    /// A blocking task panicked or was cancelled.
    #[error("backend task failed: {0}")]
    Join(String),

    /// The operation is not available on this backend.
    #[error("operation not supported by this backend: {0}")]
    Unsupported(&'static str),

    /// A workspace registry entry or selection was invalid (e.g. an unknown
    /// workspace id).
    #[error("invalid workspace configuration: {0}")]
    InvalidWorkspace(String),

    /// The current directory is inside another workspace than the one the server serves
    /// (#215): acting would write where the user is not looking.
    #[error(
        "the current directory is inside the workspace {cwd_workspace}, but the server serves \
         {current}; run `workspace select {cwd_workspace}` to switch"
    )]
    OtherWorkspace {
        /// The workspace the current directory is in.
        cwd_workspace: std::path::PathBuf,
        /// The workspace the server serves.
        current: std::path::PathBuf,
    },

    /// The server has no workspace yet.
    #[error(
        "the server has no workspace yet; run `workspace init <dir>` or `workspace select <dir>`"
    )]
    NoWorkspace,
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Join(e.to_string())
    }
}

/// Convenience alias for backend results.
pub type Result<T> = std::result::Result<T, Error>;
