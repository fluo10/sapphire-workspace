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

    #[cfg(feature = "git")]
    #[error(transparent)]
    Git(#[from] git2::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
