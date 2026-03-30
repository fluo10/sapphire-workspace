//! Vector store abstraction.
//!
//! [`VectorStore`] is a **synchronous** trait used internally by the SQLite vec
//! backend of [`crate::db::RetrieveDb`].  The LanceDB full backend
//! (`lancedb_store`) does not use this trait.
//!
//! # Chunk identity
//!
//! Each chunk is identified by the pair `(doc_id, chunk_index)`.  `doc_id` is a
//! stable i64 assigned by the caller (e.g. a path hash or application-level ID).
//! `chunk_index` is reproducibly derived from the paragraph order of the body.

use std::collections::HashSet;

use crate::error::Result;

// ── public types ──────────────────────────────────────────────────────────────

/// A single paragraph-level chunk derived from a document, ready to be embedded.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Stable document ID assigned by the caller.
    pub doc_id: i64,
    /// Zero-based position of this paragraph in the document body.
    pub chunk_index: usize,
    /// Embeddable text: title prepended to the paragraph body.
    pub text: String,
    /// Denormalised document title (for display in search results).
    pub doc_title: String,
    /// Denormalised absolute file path (for display in search results).
    pub doc_path: String,
}

/// A result returned by [`VectorStore::search_similar`].
#[derive(Debug, Clone)]
pub struct ChunkSearchResult {
    pub doc_id: i64,
    pub doc_title: String,
    pub doc_path: String,
    /// Position of the matching chunk within the document (0-based).
    pub chunk_index: usize,
    /// The text of the matching chunk.
    pub chunk_text: String,
    /// L2 distance (lower = more similar).
    pub score: f64,
}

/// Statistics about the vector index.
pub struct VecInfo {
    /// Embedding dimension (number of f32 values per vector).
    pub embedding_dim: u32,
    /// Number of chunks that have an embedding stored.
    pub vector_count: u64,
    /// Number of chunks that do not yet have an embedding.
    pub pending_count: u64,
}

// ── trait ─────────────────────────────────────────────────────────────────────

/// Abstraction over a vector storage backend.
///
/// All methods are **synchronous**.  Backends that are inherently async
/// (e.g. LanceDB) wrap their async operations in an internal Tokio runtime.
pub trait VectorStore {
    /// Return the `(doc_id, chunk_index)` pairs that already have embeddings
    /// stored, so callers can compute the pending set.
    fn embedded_chunk_keys(&self) -> Result<HashSet<(i64, usize)>>;

    /// Store embeddings for a batch of chunks.
    ///
    /// `chunks` and `embeddings` are parallel slices of equal length.
    fn insert_embeddings(&self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()>;

    /// Find the `limit` most similar chunks to `query_vec`, ordered by
    /// ascending distance.
    fn search_similar(&self, query_vec: &[f32], limit: usize) -> Result<Vec<ChunkSearchResult>>;
}

// ── internal helpers shared by db.rs and lancedb_store.rs ─────────────────────

/// Serialize a float slice to the little-endian bytes expected by sqlite-vec.
pub(crate) fn vec_serialize(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
