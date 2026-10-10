//! The embedding hook: how the bridge answers `embed.info` and `embed.embed` without
//! knowing any model.
//!
//! The model lives in the binary that runs the bridge, which injects an [`EmbedFactory`].
//! This crate — and the facade that re-exports it — stays free of model dependencies. The
//! bridge reads the settings ([`embed_settings`](crate::embed_settings)), and asks the
//! factory for a provider whenever what they resolve to changes: after a settings call, and
//! on every status tick, which is how a model set on another device of the workgroup arrives.

use std::sync::{Arc, Mutex, RwLock};

use sapphire_bridge_api::{EmbedModelInfo, EmbedNote};

use crate::embed_settings::{self, EmbedConfig, Resolved};
use crate::error::Result;
use crate::{Bridge, BridgeDir};

/// Computes embeddings for the bridge's clients.
#[async_trait::async_trait]
pub trait EmbedProvider: Send + Sync {
    /// `None` when embedding is disabled on this host.
    fn info(&self) -> Option<EmbedModelInfo>;
    /// Whether the model is in memory now (REST providers: always true).
    fn loaded(&self) -> bool;
    /// One vector per text, in order. An `Err` reaches the client as its message.
    async fn embed(&self, texts: Vec<String>) -> std::result::Result<Vec<Vec<f32>>, String>;
}

/// Builds the provider for a resolved configuration. Called again whenever it changes; the
/// bridge directory is there for the host's defaults (the local model's cache).
pub type EmbedFactory =
    Arc<dyn Fn(&EmbedConfig, &BridgeDir) -> Option<Arc<dyn EmbedProvider>> + Send + Sync>;

/// The bridge's embedding: the provider in use, and what it was built from.
pub(crate) struct EmbedState {
    /// Builds providers from the settings; `None` for a bridge without embedding, or with a
    /// fixed provider.
    factory: Option<EmbedFactory>,
    /// Set by [`Bridge::embed_provider`]: never replaced by the settings.
    fixed: bool,
    /// What answers `embed.*` now.
    provider: RwLock<Option<Arc<dyn EmbedProvider>>>,
    /// The configuration `provider` was built from. Held across a reload, so two reloads
    /// never build twice.
    applied: Mutex<Option<EmbedConfig>>,
    /// The settings as last resolved.
    resolved: Mutex<Resolved>,
    /// Whether this CPU has AVX2 (overridable for tests).
    avx2: bool,
}

impl Default for EmbedState {
    fn default() -> Self {
        EmbedState {
            factory: None,
            fixed: false,
            provider: RwLock::new(None),
            applied: Mutex::new(None),
            resolved: Mutex::new(Resolved::default()),
            avx2: embed_settings::avx2(),
        }
    }
}

impl Bridge {
    /// Answer `embed.info` and `embed.embed` with this provider, whatever the settings say.
    ///
    /// For tests, and for an in-process host that manages its own model.
    pub fn embed_provider(mut self, provider: Arc<dyn EmbedProvider>) -> Bridge {
        self.embed.fixed = true;
        self.embed.provider = RwLock::new(Some(provider));
        self
    }

    /// Build the embedding provider from the settings with this factory.
    pub fn embed_factory(mut self, factory: EmbedFactory) -> Bridge {
        self.embed.factory = Some(factory);
        self
    }

    /// Resolve the local slot's auto switch as if this CPU did (or did not) have AVX2.
    #[cfg(any(test, feature = "test-util"))]
    pub fn assume_avx2(mut self, avx2: bool) -> Bridge {
        self.embed.avx2 = avx2;
        self
    }

    /// The provider answering `embed.*` now.
    pub(crate) fn embedder(&self) -> Option<Arc<dyn EmbedProvider>> {
        self.embed.provider.read().expect("embed provider").clone()
    }

    /// Why embedding is off, or what it lacks. Only a bridge that builds its provider from
    /// the settings has anything to say.
    pub(crate) fn embed_note(&self) -> Option<EmbedNote> {
        if self.embed.factory.is_none() || self.embed.fixed {
            return None;
        }
        self.embed
            .resolved
            .lock()
            .expect("embed resolved")
            .note
            .clone()
    }

    /// Read the settings again, and rebuild the provider if what they resolve to changed.
    pub(crate) fn reload_embed(&self) -> Result<Resolved> {
        let mut applied = self.embed.applied.lock().expect("embed applied");
        let (resolved, config) = embed_settings::load(&self.dir, self.embed.avx2)?;
        *self.embed.resolved.lock().expect("embed resolved") = resolved.clone();
        if self.embed.fixed {
            return Ok(resolved);
        }
        let Some(factory) = &self.embed.factory else {
            return Ok(resolved);
        };
        if *applied == config {
            return Ok(resolved);
        }
        let provider = config.as_ref().and_then(|c| factory(c, &self.dir));
        match (&config, &provider) {
            (Some(EmbedConfig::Local { model, .. }), Some(_)) => tracing::info!(
                target: crate::logging::BRIDGE_TARGET,
                "embedding with the local model {} ({} dimensions)",
                model.model,
                model.dimension
            ),
            (Some(EmbedConfig::Remote { model, .. }), Some(_)) => tracing::info!(
                target: crate::logging::BRIDGE_TARGET,
                "embedding with the remote model {} at {} ({} dimensions)",
                model.model,
                model.endpoint,
                model.dimension
            ),
            (Some(_), None) => tracing::warn!(
                target: crate::logging::BRIDGE_TARGET,
                "this bridge cannot build the configured embedding model; embedding is off"
            ),
            (None, _) => tracing::info!(
                target: crate::logging::BRIDGE_TARGET,
                "embedding is off: {}",
                resolved
                    .note
                    .as_ref()
                    .map_or_else(|| "no model".to_owned(), ToString::to_string)
            ),
        }
        // Callers holding the old provider finish with it; the last one drops it, which
        // ends its worker and unloads a local model.
        *self.embed.provider.write().expect("embed provider") = provider;
        *applied = config;
        Ok(resolved)
    }

    /// Reload every [`STATUS_INTERVAL`](crate::STATUS_INTERVAL), for as long as the bridge
    /// serves: a model set on another device arrives through the synced workgroup root.
    pub(crate) async fn watch_embed_settings(self: Arc<Self>) -> Result<()> {
        let mut tick = tokio::time::interval(crate::STATUS_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(err) = self.reload_embed() {
                tracing::warn!(
                    target: crate::logging::BRIDGE_TARGET,
                    "could not read the embedding settings: {err}"
                );
            }
        }
    }
}
