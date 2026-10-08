//! Internal vector-store types and helpers shared by the storage backends.

// ── public types ──────────────────────────────────────────────────────────────

/// Statistics about the vector index.
pub struct VecInfo {
    /// Embedding dimension (number of f32 values per vector).
    pub embedding_dim: u32,
    /// Number of documents that have an embedding stored.
    pub vector_count: u64,
    /// Number of documents that do not yet have an embedding.
    pub pending_count: u64,
}

// ── internal helpers shared by db.rs and redb_store.rs ───────────────────────

/// Serialize a float slice to the little-endian bytes expected by sqlite-vec.
#[allow(dead_code)]
pub(crate) fn vec_serialize(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Deserialize a little-endian `f32` blob produced by [`vec_serialize`].
#[allow(dead_code)]
pub(crate) fn vec_deserialize(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// Euclidean (L2) distance between two equal-length vectors. Lower = closer.
#[allow(dead_code)]
pub(crate) fn l2_distance(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = (*x - *y) as f64;
            d * d
        })
        .sum::<f64>()
        .sqrt()
}
