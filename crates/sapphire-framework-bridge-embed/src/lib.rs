//! Text embedding for the sapphire bridge, the per-host daemon.
//!
//! This is the bridge's embedding component, shipped with the framework so that apps which
//! embed the bridge in-process (mobile) can use it too. The daemon library
//! (`sapphire-framework-bridge`) does not depend on it: it defines an `EmbedProvider` hook,
//! and the host (the bridge binary, or an embedding app) implements it with [`EmbedService`].
//!
//! - [`settings`]: `<bridge dir>/embedding.toml`.
//! - [`template`]: the prompt template, token truncation and MRL helpers (pure).
//! - [`rest`]: an OpenAI-compatible `/v1/embeddings` provider.
//! - `local` (feature `local`): Qwen3-VL-Embedding-2B on CPU through fastembed and candle.
//! - [`service`]: one worker thread that loads the model on demand and unloads it when idle.

pub mod rest;
pub mod service;
pub mod settings;
pub mod template;

#[cfg(feature = "local")]
pub mod local;

pub use rest::{HttpPost, RestEmbedder, UreqPost};
pub use service::{Embed, EmbedService, Loader, ModelInfo};
pub use settings::{EmbeddingSettings, Provider};
pub use template::TEMPLATE_VERSION;

#[cfg(feature = "local")]
pub use local::LocalQwen;

/// Errors from settings, providers and the service.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `embedding.toml` could not be read or is invalid.
    #[error("embedding settings: {0}")]
    Settings(String),
    /// The model (or provider) could not be loaded.
    #[error("embedding model failed to load: {0}")]
    Load(String),
    /// An embedding request failed.
    #[error("embedding failed: {0}")]
    Embed(String),
    /// The worker thread is gone.
    #[error("the embedding worker has stopped")]
    Stopped,
}

pub type Result<T> = std::result::Result<T, Error>;
