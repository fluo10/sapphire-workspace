use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("no git repository found at '{path}'")]
    NoRepository { path: PathBuf },

    #[error("git repository has no working directory (bare repo?)")]
    BareRepository,

    #[error("path '{path}' is not inside the working directory '{workdir}'")]
    PathOutsideWorkdir { path: PathBuf, workdir: PathBuf },

    #[error("remote '{name}' not found")]
    RemoteNotFound { name: String },

    #[error("device record for id '{id}' not found in registry")]
    DeviceNotFound { id: uuid::Uuid },

    #[error("I/O error at '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed device record at '{path}' line {line}: {source}")]
    DeviceRecordParse {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },

    #[cfg(feature = "git")]
    #[error(transparent)]
    Git(#[from] git2::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
