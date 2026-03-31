//! Unified retrieve database: FTS5 + vector search.
//!
//! [`RetrieveDb`] is the main entry point.  It manages one of the available
//! storage backends and exposes a unified API for file tracking, document
//! management, full-text search, and vector search.
//!
//! # Choosing a backend
//!
//! Call one of the `init_*` methods after [`RetrieveDb::open`]:
//!
//! | Method | Backend | Notes |
//! |--------|---------|-------|
//! | *(none)* | SQLite FTS only | Default; no vector search |
//! | [`init_sqlite_vec`](Self::init_sqlite_vec) | SQLite + sqlite-vec | Lightweight, always available |
//! | [`init_lancedb`](Self::init_lancedb) | LanceDB (full) | Requires `lancedb-store` feature; no SQLite file |
//!
//! Without calling an `init_*` method, only FTS is available via SQLite.
//!
//! # Thread safety
//!
//! [`RetrieveDb`] is `Send + Sync`.  Each public method briefly acquires an
//! internal lock to clone the `Arc`-wrapped backend, then operates independently.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{
    embed::Embedder,
    error::Result,
    retrieve_store::RetrieveStore,
    sqlite_store::SqliteStore,
    vector_store::{ChunkSearchResult, VecInfo},
};

#[cfg(feature = "lancedb-store")]
use crate::lancedb_store::LanceDbBackend;

// Re-export types that live in retrieve_store so callers using the `db` module
// path continue to find them here.
pub use crate::retrieve_store::{Document, SearchResult};
// Re-export the SQLite schema version (used by workspace helpers and the CLI).
pub use crate::sqlite_store::SCHEMA_VERSION;

// ── backend state ─────────────────────────────────────────────────────────────

enum BackendState {
    /// SQLite backend (FTS only or FTS + sqlite-vec).
    Sqlite(Arc<SqliteStore>),
    /// Full LanceDB backend.
    #[cfg(feature = "lancedb-store")]
    LanceDb(Arc<LanceDbBackend>),
}

impl BackendState {
    /// Return the underlying [`RetrieveStore`] as a cloned `Arc`.
    fn as_store(&self) -> Arc<dyn RetrieveStore> {
        match self {
            BackendState::Sqlite(s) => Arc::clone(s) as Arc<dyn RetrieveStore>,
            #[cfg(feature = "lancedb-store")]
            BackendState::LanceDb(l) => Arc::clone(l) as Arc<dyn RetrieveStore>,
        }
    }

    fn is_uninitialized_sqlite(&self) -> bool {
        matches!(self, BackendState::Sqlite(s) if s.dim().is_none())
    }
}

// ── RetrieveDb ────────────────────────────────────────────────────────────────

/// Manages the retrieve backend for full-text and (optionally) vector search.
///
/// Create with [`RetrieveDb::open`], then call:
/// - [`upsert_document`](Self::upsert_document) / [`remove_document`](Self::remove_document) to keep the index in sync.
/// - [`rebuild_fts`](Self::rebuild_fts) after a batch of upserts/removes.
/// - [`search_fts`](Self::search_fts) for full-text search.
/// - Optionally call [`init_sqlite_vec`](Self::init_sqlite_vec) or
///   [`init_lancedb`](Self::init_lancedb), then use
///   [`embed_pending`](Self::embed_pending) and
///   [`search_similar`](Self::search_similar) for vector search.
pub struct RetrieveDb {
    db_path: PathBuf,
    backend: Mutex<BackendState>,
}

impl RetrieveDb {
    /// Open the retrieve database at `db_path`.
    ///
    /// Defaults to the SQLite FTS-only backend; the SQLite file is created
    /// lazily on first use.  Call [`init_sqlite_vec`](Self::init_sqlite_vec)
    /// or [`init_lancedb`](Self::init_lancedb) to enable vector search.
    pub fn open(db_path: &Path) -> Result<Self> {
        let store = SqliteStore::new_fts_only(db_path.to_owned());
        Ok(Self {
            db_path: db_path.to_owned(),
            backend: Mutex::new(BackendState::Sqlite(Arc::new(store))),
        })
    }

    /// Delete the existing database and create a fresh one.
    pub fn rebuild(db_path: &Path) -> Result<Self> {
        crate::sqlite_store::wipe_db_files(db_path);
        Self::open(db_path)
    }

    // ── vector backend init ───────────────────────────────────────────────────

    /// Enable the sqlite-vec vector backend with `embedding_dim` dimensions.
    ///
    /// Idempotent — if the backend is already initialised this is a no-op.
    pub fn init_sqlite_vec(&self, embedding_dim: u32) -> Result<()> {
        let mut guard = self.backend.lock().unwrap();
        if guard.is_uninitialized_sqlite() {
            let store = SqliteStore::new_with_vec(self.db_path.clone(), embedding_dim)?;
            *guard = BackendState::Sqlite(Arc::new(store));
        }
        Ok(())
    }

    /// Enable the LanceDB full backend (files + documents + chunks + vectors).
    ///
    /// When active, all data lives in LanceDB tables under `lancedb_dir`.
    /// No SQLite file is created.  Idempotent.
    #[cfg(feature = "lancedb-store")]
    pub fn init_lancedb(&self, lancedb_dir: &Path, embedding_dim: u32) -> Result<()> {
        let mut guard = self.backend.lock().unwrap();
        if guard.is_uninitialized_sqlite() {
            let backend = LanceDbBackend::new(lancedb_dir, embedding_dim)?;
            *guard = BackendState::LanceDb(Arc::new(backend));
        }
        Ok(())
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Clone the active backend as an `Arc<dyn RetrieveStore>`.
    ///
    /// The lock is released immediately after cloning the `Arc`, so long-running
    /// operations do not block other threads from checking the backend state.
    fn store(&self) -> Arc<dyn RetrieveStore> {
        self.backend.lock().unwrap().as_store()
    }

    // ── document management ───────────────────────────────────────────────────

    /// Insert or replace a document in the retrieve database.
    ///
    /// Also re-chunks the document body and updates the chunks table.
    /// Stale embeddings for changed chunks are removed automatically.
    ///
    /// Call [`rebuild_fts`](Self::rebuild_fts) after a batch of upserts.
    pub fn upsert_document(&self, doc: &Document) -> Result<()> {
        self.store().upsert_document(doc)
    }

    /// Remove a document (and its chunks / embeddings) from the database.
    pub fn remove_document(&self, id: i64) -> Result<()> {
        self.store().remove_document(id)
    }

    /// Rebuild the FTS index from the current `documents` table.
    ///
    /// Call this after a batch of [`upsert_document`](Self::upsert_document)
    /// calls or whenever the FTS index may be out of date.
    pub fn rebuild_fts(&self) -> Result<()> {
        self.store().rebuild_fts()
    }

    // ── search ────────────────────────────────────────────────────────────────

    /// Full-text search.
    ///
    /// SQLite mode uses the FTS5 trigram index (substring / CJK aware).
    /// LanceDB mode uses the ngram tokenizer (better BM25 ranking).
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.store().search_fts(query, limit)
    }

    /// Find the `limit` most similar chunks to `query_vec`.
    ///
    /// Returns an empty `Vec` when no vector backend is configured.
    pub fn search_similar(
        &self,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<ChunkSearchResult>> {
        self.store().search_similar(query_vec, limit)
    }

    /// Deduplicate chunk search results to one `SearchResult` per document.
    ///
    /// When multiple chunks from the same document match, only the best-scoring
    /// (lowest L2 distance) chunk is kept.  Returns up to `limit` results
    /// ordered by ascending score.
    pub fn dedup_chunk_results(
        results: Vec<ChunkSearchResult>,
        limit: usize,
    ) -> Vec<SearchResult> {
        let mut best: HashMap<i64, ChunkSearchResult> = HashMap::new();
        for r in results {
            best.entry(r.doc_id)
                .and_modify(|e| {
                    if r.score < e.score {
                        *e = r.clone();
                    }
                })
                .or_insert(r);
        }

        let mut deduped: Vec<_> = best.into_values().collect();
        deduped.sort_by(|a, b| {
            a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal)
        });
        deduped.truncate(limit);
        deduped
            .into_iter()
            .map(|r| SearchResult {
                id: r.doc_id,
                title: r.doc_title,
                path: r.doc_path,
                score: r.score,
            })
            .collect()
    }

    // ── embedding ─────────────────────────────────────────────────────────────

    /// Generate and store embeddings for all pending (unembedded) chunks.
    ///
    /// Returns the number of newly embedded chunks.  Returns 0 immediately
    /// when no vector backend is configured.
    ///
    /// `on_progress(done, total)` is called after each batch of 100 chunks.
    pub fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        self.store().embed_pending(embedder, &on_progress)
    }

    /// Read vector index statistics.
    pub fn vec_info(&self) -> Result<VecInfo> {
        self.store().vec_info()
    }

    /// Return the IDs of all documents in the database.
    pub fn document_ids(&self) -> Result<Vec<i64>> {
        self.store().document_ids()
    }

    /// Return the total number of documents in the database.
    pub fn document_count(&self) -> Result<u64> {
        self.store().document_count()
    }

    // ── file tracking ─────────────────────────────────────────────────────────

    /// Return all tracked (path, file_mtime) pairs.
    pub fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        self.store().file_mtimes()
    }

    /// Insert or replace a file record.
    pub fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        self.store().upsert_file(path, mtime)
    }

    /// Delete a file record.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.store().remove_file(path)
    }

    /// Return the total number of tracked files.
    pub fn file_count(&self) -> Result<u64> {
        self.store().file_count()
    }
}
