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
//! | *(none)* | SQLite FTS only | Default when `sqlite-store` is enabled |
//! | [`init_sqlite_vec`](Self::init_sqlite_vec) | SQLite + sqlite-vec | Lightweight; requires `sqlite-store` |
//! | [`init_lancedb`](Self::init_lancedb) | LanceDB (full) | Requires `lancedb-store` feature; no SQLite file |
//!
//! When neither `sqlite-store` nor `lancedb-store` is compiled in, an
//! in-memory backend is used: documents are stored in a `HashMap` and
//! a simple substring search is available.  Data is not persisted to disk.
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
    vector_store::{ChunkSearchResult, VecInfo},
};

pub use crate::retrieve_store::{Document, SearchResult};

#[cfg(feature = "sqlite-store")]
use crate::sqlite_store::SqliteStore;

#[cfg(feature = "lancedb-store")]
use crate::lancedb_store::LanceDbBackend;

// Re-export the SQLite schema version (used by workspace helpers and the CLI).
#[cfg(feature = "sqlite-store")]
pub use crate::sqlite_store::SCHEMA_VERSION;

// ── in-memory backend ─────────────────────────────────────────────────────────

/// In-memory backend used when no persistent storage feature is compiled in.
///
/// All data lives in `HashMap`s and is lost when the process exits.
/// FTS is implemented as a simple case-insensitive substring scan.
/// This backend is also the *initial* state when the [`lancedb-store`] feature
/// is enabled but [`RetrieveDb::init_lancedb`] has not yet been called.
struct InMemoryStore {
    state: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    files: HashMap<String, i64>,
    documents: HashMap<i64, Document>,
}

impl InMemoryStore {
    fn new() -> Self {
        Self {
            state: Mutex::new(InMemoryState::default()),
        }
    }
}

impl RetrieveStore for InMemoryStore {
    fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        Ok(self.state.lock().unwrap().files.clone())
    }

    fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(path.to_owned(), mtime);
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.state.lock().unwrap().files.remove(path);
        Ok(())
    }

    fn file_count(&self) -> Result<u64> {
        Ok(self.state.lock().unwrap().files.len() as u64)
    }

    fn upsert_document(&self, doc: &Document) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .documents
            .insert(doc.id, doc.clone());
        Ok(())
    }

    fn remove_document(&self, id: i64) -> Result<()> {
        self.state.lock().unwrap().documents.remove(&id);
        Ok(())
    }

    fn rebuild_fts(&self) -> Result<()> {
        Ok(())
    }

    fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let state = self.state.lock().unwrap();
        let q = query.to_lowercase();
        let mut results: Vec<SearchResult> = state
            .documents
            .values()
            .filter(|doc| {
                doc.title.to_lowercase().contains(&q) || doc.body.to_lowercase().contains(&q)
            })
            .take(limit)
            .map(|doc| SearchResult {
                id: doc.id,
                title: doc.title.clone(),
                path: doc.path.clone(),
                score: 0.0,
            })
            .collect();
        results.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(results)
    }

    fn document_ids(&self) -> Result<Vec<i64>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .documents
            .keys()
            .copied()
            .collect())
    }

    fn document_count(&self) -> Result<u64> {
        Ok(self.state.lock().unwrap().documents.len() as u64)
    }

    fn embed_pending(
        &self,
        _embedder: &dyn Embedder,
        _on_progress: &dyn Fn(usize, usize),
    ) -> Result<usize> {
        Ok(0)
    }

    fn vec_info(&self) -> Result<VecInfo> {
        Ok(VecInfo {
            embedding_dim: 0,
            vector_count: 0,
            pending_count: 0,
        })
    }

    fn search_similar(&self, _query_vec: &[f32], _limit: usize) -> Result<Vec<ChunkSearchResult>> {
        Ok(vec![])
    }
}

// ── backend state ─────────────────────────────────────────────────────────────

enum BackendState {
    /// In-memory backend — data is not persisted.
    ///
    /// Used when neither `sqlite-store` nor `lancedb-store` is compiled in,
    /// and as the initial state before [`RetrieveDb::init_lancedb`] is called.
    InMemory(Arc<InMemoryStore>),
    /// SQLite backend (FTS only or FTS + sqlite-vec).
    #[cfg(feature = "sqlite-store")]
    Sqlite(Arc<SqliteStore>),
    /// Full LanceDB backend.
    #[cfg(feature = "lancedb-store")]
    LanceDb(Arc<LanceDbBackend>),
}

impl BackendState {
    /// Return the underlying [`RetrieveStore`] as a cloned `Arc`.
    fn as_store(&self) -> Arc<dyn RetrieveStore> {
        match self {
            BackendState::InMemory(s) => Arc::clone(s) as Arc<dyn RetrieveStore>,
            #[cfg(feature = "sqlite-store")]
            BackendState::Sqlite(s) => Arc::clone(s) as Arc<dyn RetrieveStore>,
            #[cfg(feature = "lancedb-store")]
            BackendState::LanceDb(l) => Arc::clone(l) as Arc<dyn RetrieveStore>,
        }
    }

    /// Returns `true` when the backend should be replaced by an `init_*` call.
    fn needs_init(&self) -> bool {
        match self {
            BackendState::InMemory(_) => true,
            #[cfg(feature = "sqlite-store")]
            BackendState::Sqlite(s) => s.dim().is_none(),
            #[cfg(feature = "lancedb-store")]
            BackendState::LanceDb(_) => false,
        }
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
    /// When `sqlite-store` is enabled, defaults to the SQLite FTS-only backend
    /// (the SQLite file is created lazily on first use).  When no storage
    /// feature is compiled in, opens an in-memory backend.
    ///
    /// Call [`init_sqlite_vec`](Self::init_sqlite_vec) or
    /// [`init_lancedb`](Self::init_lancedb) to enable vector search.
    pub fn open(db_path: &Path) -> Result<Self> {
        #[cfg(feature = "sqlite-store")]
        {
            let store = SqliteStore::new_fts_only(db_path.to_owned());
            return Ok(Self {
                db_path: db_path.to_owned(),
                backend: Mutex::new(BackendState::Sqlite(Arc::new(store))),
            });
        }

        #[cfg(not(feature = "sqlite-store"))]
        Ok(Self {
            db_path: db_path.to_owned(),
            backend: Mutex::new(BackendState::InMemory(Arc::new(InMemoryStore::new()))),
        })
    }

    /// Delete the existing database and create a fresh one.
    pub fn rebuild(db_path: &Path) -> Result<Self> {
        #[cfg(feature = "sqlite-store")]
        crate::sqlite_store::wipe_db_files(db_path);
        Self::open(db_path)
    }

    // ── vector backend init ───────────────────────────────────────────────────

    /// Enable the sqlite-vec vector backend with `embedding_dim` dimensions.
    ///
    /// Idempotent — if the backend is already initialised this is a no-op.
    #[cfg(feature = "sqlite-store")]
    pub fn init_sqlite_vec(&self, embedding_dim: u32) -> Result<()> {
        let mut guard = self.backend.lock().unwrap();
        if guard.needs_init() {
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
        if guard.needs_init() {
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
    /// In-memory mode uses a simple case-insensitive substring scan.
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
    pub fn dedup_chunk_results(results: Vec<ChunkSearchResult>, limit: usize) -> Vec<SearchResult> {
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
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
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
