//! Internal vector-store types shared by the SQLite and LanceDB backends.

// ── public types ──────────────────────────────────────────────────────────────

/// A single text chunk derived from a document, ready to be embedded.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Stable document ID assigned by the caller.
    pub doc_id: i64,
    /// First source line of this chunk (inclusive, 0-based).
    pub line_start: usize,
    /// Last source line of this chunk (inclusive, 0-based).
    pub line_end: usize,
    /// Embeddable text content of the chunk.
    pub text: String,
    /// Denormalised absolute file path (for display in search results).
    pub doc_path: String,
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

// ── internal helpers shared by db.rs and lancedb_store.rs ─────────────────────

/// Serialize a float slice to the little-endian bytes expected by sqlite-vec.
pub(crate) fn vec_serialize(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
