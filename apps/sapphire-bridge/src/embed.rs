//! Builds the bridge's embedding service from the resolved settings, through the
//! [`sapphire_bridge::EmbedProvider`] hook.

use std::path::PathBuf;
use std::sync::Arc;

use sapphire_bridge::{BridgeDir, EmbedConfig, EmbedFactory, EmbedProvider};
use sapphire_bridge_api::EmbedModelInfo;
use sapphire_framework_bridge_embed::EmbedService;

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
            revision: None,
            max_tokens: i.max_tokens,
        })
    }

    fn loaded(&self) -> bool {
        self.0.loaded()
    }

    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.0.embed(texts).await.map_err(|e| e.to_string())
    }
}

/// Where the local model's files are cached unless this device's settings say otherwise.
fn model_cache(dir: &BridgeDir) -> PathBuf {
    dirs::cache_dir()
        .map(|d| d.join("sapphire-bridge").join("models"))
        .unwrap_or_else(|| dir.root.join("models"))
}

/// The factory the binary passes to the bridge: one service per configuration. The bridge
/// calls it again whenever the settings resolve to something else.
pub fn factory() -> EmbedFactory {
    Arc::new(|config, dir| {
        let service = match config {
            EmbedConfig::Local { model, cache_dir } => {
                EmbedService::local(model, cache_dir.clone().unwrap_or_else(|| model_cache(dir)))
            }
            EmbedConfig::Remote { model, key } => EmbedService::remote(model, key.clone()),
        };
        Some(Arc::new(ServiceProvider(service)) as Arc<dyn EmbedProvider>)
    })
}

#[cfg(test)]
mod tests {
    use sapphire_bridge_api::{ApiKey, LocalModel, RemoteModel};

    use super::*;

    fn build(config: &EmbedConfig) -> Arc<dyn EmbedProvider> {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().to_path_buf()).unwrap();
        factory()(config, &dir).expect("a provider")
    }

    #[test]
    fn the_remote_slot_gives_a_rest_provider() {
        let p = build(&EmbedConfig::Remote {
            model: RemoteModel {
                endpoint: "http://127.0.0.1:9".into(),
                model: "m".into(),
                dimension: 8,
            },
            key: Some(ApiKey::new("k")),
        });
        let info = p.info().unwrap();
        assert_eq!((info.model.as_str(), info.dimension), ("m", 8));
        assert!(!p.loaded(), "nothing is loaded before the first request");
    }

    #[test]
    fn the_local_slot_reports_its_model_without_loading_it() {
        let p = build(&EmbedConfig::Local {
            model: LocalModel {
                dimension: 512,
                ..LocalModel::default()
            },
            cache_dir: None,
        });
        assert_eq!(p.info().unwrap().dimension, 512);
        assert!(!p.loaded());
    }
}
