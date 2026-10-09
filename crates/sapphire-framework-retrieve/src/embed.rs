//! The text embedding interface.
//!
//! Converts text into float vectors used for semantic similarity search.
//! This crate only defines the [`Embedder`] trait; the providers (local
//! inference and OpenAI-compatible REST) live in the bridge's embedding
//! component (`sapphire-framework-bridge-embed`).

use crate::error::Result;

/// Abstraction over a text embedding provider.
pub trait Embedder: Send + Sync {
    /// Generate embeddings for a batch of texts.
    ///
    /// Returns one `Vec<f32>` per input text, in the same order.
    /// Returns an empty `Vec` when `texts` is empty.
    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}
