//! Unified retrieve store trait.
//!
//! [`RetrieveStore`] is a **synchronous** trait that abstracts over all
//! storage backends (SQLite-vec, LanceDB, and future backends such as
//! SurrealDB).
//!
//! # Implementing a custom backend
//!
//! Implement [`RetrieveStore`] for your struct, then pass an `Arc` of it to
//! the relevant `init_*` method on [`crate::db::RetrieveDb`], or build a
//! `RetrieveDb` directly from it (see [`crate::db::RetrieveDb::from_backend`]).
//!
//! All methods are **synchronous**.  Async backends must wrap their async
//! operations inside a dedicated Tokio runtime.

use std::collections::HashMap;
use std::path::Path;

use crate::{
    embed::Embedder,
    error::Result,
    vector_store::{ChunkSearchResult, VecInfo},
};

// ── query structs ────────────────────────────────────────────────────────────

/// Full-text search query.
#[derive(Debug, Clone)]
pub struct FtsQuery<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub path_prefix: Option<&'a Path>,
}

impl<'a> FtsQuery<'a> {
    pub fn new(query: &'a str) -> Self {
        Self {
            query,
            limit: 10,
            path_prefix: None,
        }
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = n;
        self
    }

    pub fn path_prefix(mut self, p: &'a Path) -> Self {
        self.path_prefix = Some(p);
        self
    }
}

/// Vector similarity search query (chunk-level).
#[derive(Debug, Clone)]
pub struct VectorQuery<'a> {
    pub query_vec: &'a [f32],
    pub limit: usize,
    pub path_prefix: Option<&'a Path>,
}

impl<'a> VectorQuery<'a> {
    pub fn new(query_vec: &'a [f32]) -> Self {
        Self {
            query_vec,
            limit: 10,
            path_prefix: None,
        }
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = n;
        self
    }

    pub fn path_prefix(mut self, p: &'a Path) -> Self {
        self.path_prefix = Some(p);
        self
    }
}

/// Hybrid (FTS + vector) search query, merged via Reciprocal Rank Fusion.
#[derive(Debug, Clone)]
pub struct HybridQuery<'a> {
    pub text: &'a str,
    pub query_vec: &'a [f32],
    pub limit: usize,
    pub path_prefix: Option<&'a Path>,
    pub rrf_k: f64,
    pub weight_fts: f64,
    pub weight_sem: f64,
}

impl<'a> HybridQuery<'a> {
    pub fn new(text: &'a str, query_vec: &'a [f32]) -> Self {
        Self {
            text,
            query_vec,
            limit: 10,
            path_prefix: None,
            rrf_k: 60.0,
            weight_fts: 1.0,
            weight_sem: 1.0,
        }
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = n;
        self
    }

    pub fn path_prefix(mut self, p: &'a Path) -> Self {
        self.path_prefix = Some(p);
        self
    }

    pub fn rrf_k(mut self, k: f64) -> Self {
        self.rrf_k = k;
        self
    }

    pub fn weight_fts(mut self, w: f64) -> Self {
        self.weight_fts = w;
        self
    }

    pub fn weight_sem(mut self, w: f64) -> Self {
        self.weight_sem = w;
        self
    }
}

// ── shared domain types ───────────────────────────────────────────────────────

/// A document to be indexed for FTS and/or vector search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Document {
    /// Stable identifier assigned by the caller.
    ///
    /// For sapphire-journal, this is the `CarettaId` stored as `i64`.  For
    /// other applications, a hash of the file path works well.
    pub id: i64,
    /// Human-readable title (shown in search results).
    pub title: String,
    /// Full body text (indexed by FTS and chunked for vector embedding).
    pub body: String,
    /// Absolute file path (shown in search results).
    pub path: String,
    /// Pre-computed text chunks with source-location positions.
    ///
    /// Each element is `(line, column, embed_text)` where:
    ///
    /// - `line` — 0-based source line number stored as the `line` column in the
    ///   database.  Returned verbatim in
    ///   [`ChunkSearchResult::line`](crate::vector_store::ChunkSearchResult::line)
    ///   so a GUI can navigate directly to the source location.
    /// - `column` — 0-based byte offset within `line`.
    /// - `embed_text` — title-prepended chunk text used for vector embedding
    ///   (the same format that [`crate::chunker::chunk_document`] produces).
    ///
    /// When `None`, the storage backend falls back to auto-chunking `body` via
    /// [`crate::chunker::chunk_document`] with sequential 0-based `line` values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<(usize, usize, String)>>,
}

/// A search result from [`RetrieveStore::search_fts`] or a deduplicated
/// result from [`crate::db::RetrieveDb::dedup_chunk_results`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub id: i64,
    pub title: String,
    pub path: String,
    /// BM25 rank (FTS; negative: more-negative = more relevant) or
    /// L2 distance (vector; lower = more similar).
    pub score: f64,
}

// ── trait ─────────────────────────────────────────────────────────────────────

/// Unified synchronous interface for retrieve storage backends.
///
/// All methods are **synchronous**.  Async backends (e.g. LanceDB) wrap their
/// async operations in an internal Tokio runtime.
///
/// Built-in implementations:
/// - [`SqliteStore`](crate::sqlite_store::SqliteStore) — SQLite-vec backend
///   (always available, lightweight, default).
/// - [`LanceDbBackend`](crate::lancedb_store::LanceDbBackend) — full LanceDB
///   backend (requires the `lancedb-store` feature).
///
/// # Adding a new backend (e.g. SurrealDB)
///
/// 1. Implement `RetrieveStore` for your type.
/// 2. Call [`RetrieveDb::from_backend`](crate::db::RetrieveDb::from_backend)
///    with an `Arc` of your implementation.
pub trait RetrieveStore: Send + Sync {
    // ── file tracking ──────────────────────────────────────────────────────────

    /// Return all tracked `(path, mtime)` pairs.
    fn file_mtimes(&self) -> Result<HashMap<String, i64>>;

    /// Insert or replace a file record.
    fn upsert_file(&self, path: &str, mtime: i64) -> Result<()>;

    /// Delete a file record.
    fn remove_file(&self, path: &str) -> Result<()>;

    /// Return the total number of tracked files.
    fn file_count(&self) -> Result<u64>;

    // ── document management ────────────────────────────────────────────────────

    /// Insert or replace a document (also re-chunks the body).
    ///
    /// Call [`rebuild_fts`](Self::rebuild_fts) after a batch of upserts.
    fn upsert_document(&self, doc: &Document) -> Result<()>;

    /// Remove a document and its chunks / embeddings.
    ///
    /// Performs an incremental FTS delete so a full rebuild is not needed
    /// for single removals.
    fn remove_document(&self, id: i64) -> Result<()>;

    /// Rebuild the FTS index from the current documents table.
    ///
    /// Call this after a batch of [`upsert_document`](Self::upsert_document)
    /// calls or whenever the FTS index may be out of date.
    fn rebuild_fts(&self) -> Result<()>;

    /// Full-text search; returns up to `limit` results ordered by relevance.
    ///
    /// When `q.path_prefix` is set, only documents whose path starts with
    /// the given prefix are returned.
    fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<SearchResult>>;

    /// Return the IDs of all documents in the database.
    fn document_ids(&self) -> Result<Vec<i64>>;

    /// Return the total number of documents.
    fn document_count(&self) -> Result<u64>;

    // ── vector / embedding ─────────────────────────────────────────────────────

    /// Generate and store embeddings for all pending (unembedded) chunks.
    ///
    /// `on_progress(done, total)` is called after each batch of 100 chunks.
    /// Returns the number of newly embedded chunks (0 when no vector backend
    /// is configured).
    fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<usize>;

    /// Return vector index statistics.
    fn vec_info(&self) -> Result<VecInfo>;

    /// Find the `limit` most similar chunks to `query_vec`, ordered by
    /// ascending distance.
    ///
    /// When `q.path_prefix` is set, only chunks belonging to documents
    /// whose path starts with the given prefix are returned.
    ///
    /// Returns an empty `Vec` when no vector backend is configured.
    fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<ChunkSearchResult>>;

    /// Hybrid search combining FTS and vector similarity via RRF.
    ///
    /// The default implementation calls [`search_fts`](Self::search_fts) and
    /// [`search_similar`](Self::search_similar), deduplicates chunks, and
    /// merges via [`crate::db::merge_rrf`]. Backends may override for
    /// native hybrid support.
    fn search_hybrid(&self, q: &HybridQuery<'_>) -> Result<Vec<SearchResult>> {
        crate::db::default_hybrid(self, q)
    }
}
