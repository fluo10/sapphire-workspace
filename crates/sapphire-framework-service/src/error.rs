//! Errors raised while installing a service.

use thiserror::Error;

/// Errors raised while installing a service.
#[derive(Debug, Error)]
pub enum Error {
    /// A file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The OS service manager refused something.
    #[error("the service manager failed: {0}")]
    Manager(String),

    /// The install request contradicts itself.
    #[error("{0}")]
    Config(String),
}

/// Convenience alias for service-install results.
pub type Result<T> = std::result::Result<T, Error>;
