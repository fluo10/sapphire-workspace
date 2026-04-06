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

use crate::{
    embed::Embedder,
    error::Result,
    vector_store::{ChunkSearchResult, VecInfo},
};

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
    /// SQLite mode uses the FTS5 trigram index (substring / CJK aware).
    /// LanceDB mode uses the ngram tokenizer (better BM25 ranking).
    fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>>;

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
    /// Returns an empty `Vec` when no vector backend is configured.
    fn search_similar(&self, query_vec: &[f32], limit: usize) -> Result<Vec<ChunkSearchResult>>;
}
