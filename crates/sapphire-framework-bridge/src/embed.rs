//! The embedding hook: how the bridge answers `embed.info` and `embed.embed` without
//! knowing any model.
//!
//! The model lives in the binary that runs the bridge, which injects an [`EmbedProvider`].
//! This crate — and the facade that re-exports it — stays free of model dependencies.

use std::sync::Arc;

use sapphire_bridge_api::EmbedModelInfo;

use crate::BridgeDir;

/// Computes embeddings for the bridge's clients.
#[async_trait::async_trait]
pub trait EmbedProvider: Send + Sync {
    /// `None` when embedding is disabled on this host.
    fn info(&self) -> Option<EmbedModelInfo>;
    /// Whether the model is in memory now (REST providers: always true).
    fn loaded(&self) -> bool;
    /// One vector per text, in order. An `Err` reaches the client as its message.
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String>;
}

/// Builds the provider once the bridge directory is known (the binary reads `embedding.toml`).
pub type EmbedFactory = Box<dyn FnOnce(&BridgeDir) -> Option<Arc<dyn EmbedProvider>> + Send>;
