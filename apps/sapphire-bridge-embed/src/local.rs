//! Qwen3-VL-Embedding-2B on CPU, text mode, through fastembed's `qwen3` (candle) backend.
//!
//! fastembed applies no template and truncates after its own tokenization, so this provider
//! truncates the content by tokens first, then wraps it in the template ([`wrap`]), and sets
//! the model's `max_length` to `max_tokens + 64` so the template is never cut.
//!
//! The files are fetched through hf-hub into an explicit cache directory. fastembed's own
//! `from_hf` would pick `HF_HOME`, `FASTEMBED_CACHE_DIR` or `./.fastembed_cache`, which for a
//! service means its working directory; so the model is assembled here the way `from_hf`
//! does it (fastembed 7.1.1, `models/qwen3.rs`).

use std::path::{Path, PathBuf};

use candle_core::{DType, Device};
use candle_nn::VarBuilder;
use fastembed::{Qwen3Config, Qwen3Model, Qwen3TextEmbedding};
use hf_hub::api::sync::{ApiBuilder, ApiRepo};
use tokenizers::{PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::service::Embed;
use crate::settings::{EmbeddingSettings, LOCAL_MODEL};
use crate::template::{mrl, truncate_at, wrap};
use crate::{Error, Result};

/// Room for the template's tokens on top of `max_tokens`.
const TEMPLATE_TOKENS: usize = 64;

/// Texts per forward pass. Inputs are padded to the longest in a pass, and attention memory
/// grows with batch × length², so passes stay small even when the service hands over 32.
const FORWARD_BATCH: usize = 8;

/// Qwen3-VL-Embedding-2B with this crate's template, truncation and MRL.
pub struct LocalQwen {
    model: Qwen3TextEmbedding,
    /// The model's tokenizer, without the padding and truncation the model's copy has.
    tokenizer: Tokenizer,
    max_tokens: usize,
    dimension: usize,
}

impl LocalQwen {
    /// Load the model at f32 on the CPU. The files are downloaded into `cache_dir` (hf-hub
    /// layout) on first use: about 4 GB, about 7 GB resident once loaded.
    pub fn load(settings: &EmbeddingSettings, cache_dir: &Path) -> Result<Self> {
        let load_err = |what: &str, e: &dyn std::fmt::Display| {
            Error::Load(format!("{LOCAL_MODEL}: {what}: {e}"))
        };
        let mut builder = ApiBuilder::new()
            .with_cache_dir(cache_dir.to_path_buf())
            .with_progress(false);
        if let Some(token) = std::env::var("HF_TOKEN")
            .ok()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
        {
            builder = builder.with_token(Some(token));
        }
        let repo = builder
            .build()
            .map_err(|e| load_err("hf-hub", &e))?
            .model(LOCAL_MODEL.to_owned());

        let config_path = repo
            .get("config.json")
            .map_err(|e| load_err("config.json", &e))?;
        let config_bytes = std::fs::read(&config_path).map_err(|e| load_err("config.json", &e))?;
        let (config, prefix) = parse_config_and_weight_prefix(&config_bytes)?;
        let weights = safetensors_weight_files(&repo)?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| load_err("tokenizer.json", &e))?;

        // SAFETY: the safetensors files are memory-mapped read-only. They live in the hf-hub
        // cache, which nothing in this process writes while the model is loaded.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&weights, DType::F32, &Device::Cpu)
                .map_err(|e| load_err("weights", &e))?
        };
        let vb = match prefix {
            Some(prefix) => vb.pp(prefix),
            None => vb,
        };
        let model = Qwen3Model::new(config, vb).map_err(|e| load_err("model", &e))?;

        let tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| load_err("tokenizer.json", &e))?;
        let mut model_tokenizer = tokenizer.clone();
        // Left padding keeps the real last token at the last position (last-token pooling).
        model_tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Left,
            ..Default::default()
        }));
        model_tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: settings.max_tokens + TEMPLATE_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| load_err("tokenizer truncation", &e))?;

        Ok(Self {
            model: Qwen3TextEmbedding::new(model, model_tokenizer),
            tokenizer,
            max_tokens: settings.max_tokens,
            dimension: settings.dimension as usize,
        })
    }

    /// `text` truncated to `max_tokens` content tokens and wrapped in the template.
    fn prompt(&self, text: &str) -> Result<String> {
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| Error::Embed(format!("tokenize: {e}")))?;
        Ok(wrap(truncate_at(
            text,
            encoding.get_offsets(),
            self.max_tokens,
        )))
    }
}

/// The text config and weight prefix: a plain Qwen3 config, or a Qwen3-VL config whose text
/// model lives under `model.language_model`. Mirrors fastembed 7.1.1's private helper.
fn parse_config_and_weight_prefix(bytes: &[u8]) -> Result<(Qwen3Config, Option<&'static str>)> {
    #[derive(serde::Deserialize)]
    struct VlConfig {
        text_config: Qwen3Config,
    }
    if let Ok(config) = serde_json::from_slice::<Qwen3Config>(bytes) {
        return Ok((config, None));
    }
    if let Ok(config) = serde_json::from_slice::<VlConfig>(bytes) {
        return Ok((config.text_config, Some("model.language_model")));
    }
    Err(Error::Load(format!(
        "{LOCAL_MODEL}: config.json is neither a Qwen3 nor a Qwen3-VL text config"
    )))
}

/// `model.safetensors`, or every shard listed in `model.safetensors.index.json`.
/// Mirrors fastembed 7.1.1's private helper.
fn safetensors_weight_files(repo: &ApiRepo) -> Result<Vec<PathBuf>> {
    const SINGLE_FILE: &str = "model.safetensors";
    const INDEX_FILE: &str = "model.safetensors.index.json";

    if let Ok(path) = repo.get(SINGLE_FILE) {
        return Ok(vec![path]);
    }
    let err = |e: &dyn std::fmt::Display| Error::Load(format!("{LOCAL_MODEL}: {INDEX_FILE}: {e}"));
    let index_path = repo.get(INDEX_FILE).map_err(|e| err(&e))?;
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(index_path).map_err(|e| err(&e))?)
            .map_err(|e| err(&e))?;
    let mut shards: Vec<String> = index["weight_map"]
        .as_object()
        .ok_or_else(|| err(&"no `weight_map` object"))?
        .values()
        .filter_map(|file| file.as_str().map(str::to_owned))
        .collect();
    shards.sort_unstable();
    shards.dedup();
    shards
        .iter()
        .map(|file| {
            repo.get(file)
                .map_err(|e| Error::Load(format!("{LOCAL_MODEL}: {file}: {e}")))
        })
        .collect()
}

impl Embed for LocalQwen {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prompts = texts
            .iter()
            .map(|t| self.prompt(t))
            .collect::<Result<Vec<String>>>()?;
        let mut out = Vec::with_capacity(prompts.len());
        for batch in prompts.chunks(FORWARD_BATCH) {
            let vectors = self
                .model
                .embed(batch)
                .map_err(|e| Error::Embed(format!("{LOCAL_MODEL}: {e}")))?;
            out.extend(vectors.iter().map(|v| mrl(v, self.dimension)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    /// Downloads about 4 GB on first run and needs about 10 GB of RAM. The cache is
    /// `SAPPHIRE_EMBED_TEST_CACHE`, else `<crate>/.fastembed_cache` (gitignored).
    #[test]
    #[ignore]
    fn real_model_embeds_japanese_sensibly() {
        let settings = EmbeddingSettings {
            enabled: true,
            provider: crate::Provider::Local,
            model: LOCAL_MODEL.into(),
            dimension: 1024,
            max_tokens: 1024,
            endpoint: None,
            api_key_env: None,
            cache_dir: None,
        };
        let cache = std::env::var("SAPPHIRE_EMBED_TEST_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join(".fastembed_cache"));
        let mut model = LocalQwen::load(&settings, &cache).unwrap();
        let texts: Vec<String> = ["今日は雨が降っている", "雨の日です", "請求書の支払い期限"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let v = model.embed(&texts).unwrap();
        assert_eq!(v.len(), 3);
        for x in &v {
            assert_eq!(x.len(), 1024);
            assert!((dot(x, x).sqrt() - 1.0).abs() < 1e-4);
        }
        let rain = dot(&v[0], &v[1]);
        assert!(
            rain > dot(&v[0], &v[2]),
            "rain {rain} vs bill {}",
            dot(&v[0], &v[2])
        );
        assert!(
            rain > dot(&v[1], &v[2]),
            "rain {rain} vs bill {}",
            dot(&v[1], &v[2])
        );
        eprintln!(
            "cos: rain/rain {rain:.4}, rain/bill {:.4}, rain2/bill {:.4}",
            dot(&v[0], &v[2]),
            dot(&v[1], &v[2])
        );
    }
}
