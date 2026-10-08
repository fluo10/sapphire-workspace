//! Unified retrieve store trait.
//!
//! [`RetrieveStore`] is a **synchronous** trait that abstracts over all
//! storage backends (SQLite-vec, LanceDB, and future backends such as
//! SurrealDB).
//!
//! All methods are **synchronous**.  Async backends must wrap their async
//! operations inside a dedicated Tokio runtime.

use std::path::Path;

use crate::{embed::Embedder, error::Result, vector_store::VecInfo};

// ── query structs ────────────────────────────────────────────────────────────

/// Full-text search query.
#[derive(Debug, Clone)]
pub struct FtsQuery<'a> {
    /// Query text.
    pub query: &'a str,
    /// Maximum number of file-level results.
    pub limit: usize,
    /// When set, restrict results to documents whose `path` starts with this
    /// absolute prefix.
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

/// Vector (semantic) similarity query.
///
/// The backend embeds `query` using `embedder` internally, so callers don't
/// need to pre-compute the vector.
pub struct VectorQuery<'a> {
    pub query: &'a str,
    pub embedder: &'a dyn Embedder,
    pub limit: usize,
    pub path_prefix: Option<&'a Path>,
}

impl<'a> VectorQuery<'a> {
    pub fn new(query: &'a str, embedder: &'a dyn Embedder) -> Self {
        Self {
            query,
            embedder,
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

impl std::fmt::Debug for VectorQuery<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorQuery")
            .field("query", &self.query)
            .field("limit", &self.limit)
            .field("path_prefix", &self.path_prefix)
            .finish_non_exhaustive()
    }
}

/// Hybrid (FTS + vector) search query, merged via Reciprocal Rank Fusion.
///
/// When `embedder` is `None`, falls back to FTS-only.
pub struct HybridQuery<'a> {
    pub query: &'a str,
    pub embedder: Option<&'a dyn Embedder>,
    pub limit: usize,
    pub path_prefix: Option<&'a Path>,
    pub rrf_k: f64,
    pub weight_fts: f64,
    pub weight_sem: f64,
}

impl<'a> HybridQuery<'a> {
    pub fn new(query: &'a str) -> Self {
        Self {
            query,
            embedder: None,
            limit: 10,
            path_prefix: None,
            rrf_k: 60.0,
            weight_fts: 1.0,
            weight_sem: 1.0,
        }
    }

    pub fn embedder(mut self, e: &'a dyn Embedder) -> Self {
        self.embedder = Some(e);
        self
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

impl std::fmt::Debug for HybridQuery<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridQuery")
            .field("query", &self.query)
            .field("limit", &self.limit)
            .field("path_prefix", &self.path_prefix)
            .field("rrf_k", &self.rrf_k)
            .field("weight_fts", &self.weight_fts)
            .field("weight_sem", &self.weight_sem)
            .finish_non_exhaustive()
    }
}

// ── shared domain types ───────────────────────────────────────────────────────

/// A document to index for FTS and vector search: one file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Document {
    /// Stable identifier assigned by the caller.
    pub id: i64,
    /// The text that is indexed and embedded: the file's content.
    pub body: String,
    /// Absolute file path (shown in search results).
    pub path: String,
}

/// One file matching a search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileSearchResult {
    pub id: i64,
    pub path: String,
    /// FTS: BM25 (higher is better). Vector: L2 distance (lower is better).
    /// Hybrid: RRF (higher is better).
    pub score: f64,
    /// A short excerpt: the FTS fragment around the match, else the leading text.
    /// At most [`SNIPPET_CHARS`](crate::snippet::SNIPPET_CHARS) characters, on one line.
    pub snippet: String,
}

// ── trait ─────────────────────────────────────────────────────────────────────

/// Unified synchronous interface for retrieve storage backends.
///
/// Built-in implementations:
/// - [`RedbStore`](crate::redb_store::RedbStore) — redb records + tantivy
///   full-text index + brute-force vectors (requires the `redb-store` feature).
/// - `InMemoryStore` — ephemeral, used when no persistent backend is enabled.
pub trait RetrieveStore: Send + Sync {
    // ── document management ────────────────────────────────────────────────────

    fn upsert_document(&self, doc: &Document) -> Result<()>;
    fn remove_document(&self, id: i64) -> Result<()>;

    /// Rebuild the FTS index.  Call after a batch of upserts.
    fn rebuild_fts(&self) -> Result<()>;

    fn document_ids(&self) -> Result<Vec<i64>>;
    fn document_count(&self) -> Result<u64>;

    // ── embedding ──────────────────────────────────────────────────────────────

    /// Generate and store embeddings for all documents without a vector.
    fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<usize>;

    fn vec_info(&self) -> Result<VecInfo>;

    // ── search ─────────────────────────────────────────────────────────────────

    /// Full-text search at file granularity.
    fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<FileSearchResult>>;

    /// Semantic (vector) search at file granularity.
    ///
    /// The backend embeds `q.query` using `q.embedder` internally.
    fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<FileSearchResult>>;

    /// Hybrid search: runs FTS + vector and merges via RRF (default impl).
    ///
    /// If `q.embedder` is `None`, falls back to FTS-only.
    fn search_hybrid(&self, q: &HybridQuery<'_>) -> Result<Vec<FileSearchResult>> {
        crate::db::default_hybrid(self, q)
    }
}
