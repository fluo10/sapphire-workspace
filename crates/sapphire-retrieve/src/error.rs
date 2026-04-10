use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[cfg(feature = "sqlite-store")]
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("embedding error: {0}")]
    Embed(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "retrieve DB schema too new: DB version {db_version}, app version {app_version}; \
         delete the retrieve DB file and re-sync"
    )]
    SchemaTooNew { db_version: i32, app_version: i32 },
    #[cfg(feature = "lancedb-store")]
    #[error("LanceDB error: {0}")]
    LanceDb(#[from] lancedb::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
