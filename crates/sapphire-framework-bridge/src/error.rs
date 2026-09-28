//! What can go wrong while running the bridge.

use grain_id::GrainId;
use thiserror::Error;

/// Errors the bridge surfaces.
#[derive(Debug, Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The local control plane failed.
    #[error(transparent)]
    Ipc(#[from] sapphire_ipc::Error),

    /// A device ledger could not be read or written.
    #[error("registry: {0}")]
    Registry(#[from] sapphire_registry::Error),

    /// The bridge directory's on-disk format is unreadable, or newer than this build.
    #[error("{0}")]
    Format(String),

    /// This host has not joined a workgroup, so there is nothing to authorize against.
    #[error("this host has not joined a workgroup")]
    NoWorkgroup,

    /// A peer may not connect: it is not a device of this workgroup, or it is retired.
    #[error("{0}")]
    Unauthorized(String),

    /// No app server on this host owns that workspace.
    #[error("no app server on this host owns workspace {0}")]
    UnknownWorkspace(GrainId),

    /// Another bridge already holds the single-instance lock.
    #[error("another bridge is already running (pid {0})")]
    AlreadyRunning(u32),

    /// A configuration file is unreadable or invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// Installing, removing or reporting the bridge's service failed.
    ///
    /// Carries the service crate's own message, which already says what to do about the
    /// case at hand, or which platform offers what.
    #[error(transparent)]
    Service(#[from] sapphire_framework_service::Error),

    /// A peer sent something the data plane could not make sense of.
    #[error("{0}")]
    Protocol(String),

    /// A peer could not be reached, or a peer stream failed.
    #[error("{0}")]
    Peer(String),
}

/// Convenience alias for bridge results.
pub type Result<T> = std::result::Result<T, Error>;
