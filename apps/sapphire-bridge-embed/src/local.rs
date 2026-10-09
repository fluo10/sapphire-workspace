//! Qwen3-VL-Embedding-2B on CPU, text mode, through fastembed's `qwen3` (candle) backend.

//!
//! fastembed applies no template and truncates after its own tokenization, so this provider
//! truncates the content by tokens first, then wraps it in the template ([`wrap`]), and lets
//! fastembed's `max_length` (`max_tokens + 64`) leave the template intact.

use std::path::PathBuf;

use candle_core::{DType, Device};
use fastembed::Qwen3TextEmbedding;
use tokenizers::Tokenizer;

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
    /// The model's tokenizer, without fastembed's padding and truncation settings.
    tokenizer: Tokenizer,
    max_tokens: usize,
    dimension: usize,
}

impl LocalQwen {
    /// Load the model at f32 on the CPU, downloading the weights into the Hugging Face cache
    /// on first use (about 4 GB; about 7 GB resident once loaded).
    pub fn load(settings: &EmbeddingSettings) -> Result<Self> {
        let model = Qwen3TextEmbedding::from_hf(
            LOCAL_MODEL,
            &Device::Cpu,
            DType::F32,
            settings.max_tokens + TEMPLATE_TOKENS,
        )
        .map_err(|e| Error::Load(format!("{LOCAL_MODEL}: {e}")))?;
        let path = cached_tokenizer_path()?;
        let tokenizer = Tokenizer::from_file(&path)
            .map_err(|e| Error::Load(format!("{}: {e}", path.display())))?;
        Ok(Self {
            model,
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

/// `tokenizer.json` for [`LOCAL_MODEL`], as fastembed's `from_hf` left it in the hf-hub cache.
///
/// fastembed does not re-export hf-hub, so this repeats its cache choice: `HF_HOME`, else
/// fastembed's cache directory (`FASTEMBED_CACHE_DIR`, default `.fastembed_cache`). hf-hub's
/// offline `Cache` then follows `refs/main` to the snapshot.
fn cached_tokenizer_path() -> Result<PathBuf> {
    let dir = std::env::var("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(fastembed::get_cache_dir()));
    hf_hub::Cache::new(dir.clone())
        .model(LOCAL_MODEL.to_owned())
        .get("tokenizer.json")
        .ok_or_else(|| {
            Error::Load(format!(
                "tokenizer.json for {LOCAL_MODEL} is not in the cache at {}",
                dir.display()
            ))
        })
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

    /// Downloads about 4 GB on first run and needs about 10 GB of RAM.
    #[test]
    #[ignore]
    fn real_model_embeds_japanese_sensibly() {
        let settings = EmbeddingSettings {
            enabled: true,
            provider: crate::Provider::Local,
            model: crate::settings::LOCAL_MODEL.into(),
            dimension: 1024,
            max_tokens: 1024,
            endpoint: None,
            api_key_env: None,
        };
        let mut model = LocalQwen::load(&settings).unwrap();
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
