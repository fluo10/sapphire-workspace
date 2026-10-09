//! Reads `embedding.toml` from the bridge directory and hands the bridge an embedding
//! service through the [`sapphire_bridge::EmbedProvider`] hook.

use std::path::PathBuf;
use std::sync::Arc;

use sapphire_bridge::{BridgeDir, EmbedFactory, EmbedProvider};
use sapphire_bridge_api::EmbedModelInfo;
use sapphire_framework_bridge_embed::{EmbedService, EmbeddingSettings};

/// Adapts the embedding service to the bridge's hook.
pub struct ServiceProvider(EmbedService);

#[async_trait::async_trait]
impl EmbedProvider for ServiceProvider {
    fn info(&self) -> Option<EmbedModelInfo> {
        let i = self.0.info();
        Some(EmbedModelInfo {
            model: i.model,
            dimension: i.dimension,
            template_version: i.template_version,
        })
    }

    fn loaded(&self) -> bool {
        self.0.loaded()
    }

    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.0.embed(texts).await.map_err(|e| e.to_string())
    }
}

/// Where the local model's files are cached unless `embedding.toml` says otherwise.
fn model_cache(dir: &BridgeDir) -> PathBuf {
    dirs::cache_dir()
        .map(|d| d.join("sapphire-bridge").join("models"))
        .unwrap_or_else(|| dir.root.join("models"))
}

/// The factory the binary passes to the bridge. A missing, disabled or invalid
/// `embedding.toml` leaves embedding off; an invalid one is logged.
pub fn factory() -> EmbedFactory {
    Box::new(|dir| {
        let path = dir.root.join(EmbeddingSettings::FILE);
        match EmbeddingSettings::load(&path) {
            Ok(Some(settings)) => EmbedService::from_settings(&settings, model_cache(dir))
                .map(|svc| Arc::new(ServiceProvider(svc)) as Arc<dyn EmbedProvider>),
            Ok(None) => None,
            Err(err) => {
                tracing::error!("embedding.toml: {err}; embedding is disabled");
                None
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(body: Option<&str>) -> Option<Arc<dyn EmbedProvider>> {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().to_path_buf()).unwrap();
        if let Some(body) = body {
            std::fs::write(tmp.path().join(EmbeddingSettings::FILE), body).unwrap();
        }
        factory()(&dir)
    }

    #[test]
    fn no_file_gives_none() {
        assert!(build(None).is_none());
    }

    #[test]
    fn disabled_gives_none() {
        assert!(build(Some("enabled = false\n")).is_none());
    }

    #[test]
    fn invalid_file_gives_none() {
        assert!(build(Some("this is = not [valid toml")).is_none());
        assert!(build(Some("enabled = true\ndimension = 3\n")).is_none());
    }

    #[test]
    fn enabled_openai_gives_a_provider() {
        let p = build(Some(
            "enabled = true\nprovider = \"openai\"\nmodel = \"m\"\ndimension = 8\nendpoint = \"http://127.0.0.1:9\"\n",
        ))
        .expect("provider");
        let info = p.info().unwrap();
        assert_eq!((info.model.as_str(), info.dimension), ("m", 8));
    }
}
