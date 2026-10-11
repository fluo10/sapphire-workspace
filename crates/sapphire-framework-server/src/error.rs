use thiserror::Error;

/// Errors raised while serving an application's workspaces.
#[derive(Debug, Error)]
pub enum Error {
    /// Opening or using a workspace failed.
    #[error(transparent)]
    Workspace(#[from] sapphire_workspace::Error),

    /// A backend operation failed.
    #[error(transparent)]
    Backend(#[from] sapphire_backend::Error),

    /// The IPC layer failed.
    #[error(transparent)]
    Ipc(#[from] sapphire_ipc::Error),

    /// Listening, or preparing the runtime directory, failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The request named a directory that is not a workspace of this application.
    #[error("{0} is not a {1} workspace")]
    UnknownWorkspace(std::path::PathBuf, &'static str),

    /// The server has no workspace yet, or the one asked for is no longer current (#215).
    #[error(
        "this server has no workspace yet; run `workspace init <dir>` or `workspace select <dir>`"
    )]
    NoWorkspace,

    /// The workgroup has no workspace with this name or id.
    #[error("the workgroup has no workspace {0}")]
    UnknownWorkspaceName(String),

    /// The workgroup workspace belongs to another application.
    #[error("workspace {name} belongs to {app_name}, not this application")]
    WrongApp {
        /// The workspace's name.
        name: String,
        /// The application that owns it.
        app_name: String,
    },

    /// A workspace's sync identity could not be read or written.
    ///
    /// Never repaired by minting a new identity: the workspace may already exist on
    /// another device, and a second identity would make the two sync as unrelated
    /// workspaces for ever.
    #[error("{0}")]
    SyncId(String),

    /// The replication core failed.
    ///
    /// Carries its error's `Display`, not the error: the core's own type is an internal
    /// detail, while the message is what status and the logs need.
    #[error("replication failed: {0}")]
    Sync(String),

    /// The bridge refused a request, or could not be reached.
    ///
    /// Not fatal to the app server: the bridge carries replication, and everything the
    /// application itself does works without it (spec §10).
    #[error("the bridge could not be reached: {0}")]
    Bridge(String),

    /// A replication session failed.
    #[error(transparent)]
    Session(#[from] sapphire_framework_session::Error),

    /// A configuration file that could not be read or written as expected.
    #[error("{0}")]
    Config(String),

    /// Installing, removing or reporting this application's service failed.
    ///
    /// Carries the service crate's own message, which already says what to do about the
    /// case at hand, or which platform offers what.
    #[error(transparent)]
    Service(#[from] sapphire_framework_service::Error),
}

/// Convenience alias for server results.
pub type Result<T> = std::result::Result<T, Error>;
