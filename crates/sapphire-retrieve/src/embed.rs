//! Text embedding providers.
//!
//! Converts text into float vectors used for semantic similarity search.
//!
//! The supported providers are:
//!
//! - **`"openai"`** — OpenAI-compatible `/v1/embeddings` endpoint.
//! - **`"ollama"`** — Ollama `/api/embed` endpoint.
//! - **`"fastembed"`** — Local ONNX inference via the `fastembed` crate.
//!   No server required; model weights are downloaded from Hugging Face
//!   on first use and cached under `~/.cache/sapphire-retrieve/fastembed/`.

use crate::error::{Error, Result};

// ── configuration ─────────────────────────────────────────────────────────────

/// Runtime embedding provider configuration passed to [`build_embedder`].
///
/// This is the minimal, non-serializable config used to construct an
/// [`Embedder`] at runtime.  For the user-facing, serde-annotated config
/// see [`crate::config::EmbeddingConfig`].
#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    /// Embedding provider: `"openai"`, `"ollama"`, or `"fastembed"`.
    pub provider: String,
    /// Model name or identifier (provider-specific).
    pub model: String,
    /// Environment variable holding the API key (default: `"OPENAI_API_KEY"`).
    /// Only used by the `"openai"` provider.
    pub api_key_env: Option<String>,
    /// Base URL override for the embedding endpoint.
    /// For `"openai"`: defaults to `https://api.openai.com`.
    /// For `"ollama"`: defaults to `http://localhost:11434`.
    pub base_url: Option<String>,
    /// Directory where downloaded model weights are cached.
    /// Only used by the `"fastembed"` provider.
    /// Falls back to the OS temporary directory when `None`.
    pub cache_dir: Option<std::path::PathBuf>,
}

// ── Embedder trait ────────────────────────────────────────────────────────────

/// Abstraction over a text embedding provider.
///
/// Implementations hold any long-lived state needed for efficient repeated
/// inference (e.g. the loaded ONNX model for `fastembed`).  REST-backed
/// providers (OpenAI, Ollama) are stateless and simply store their config.
pub trait Embedder: Send + Sync {
    /// Generate embeddings for a batch of texts.
    ///
    /// Returns one `Vec<f32>` per input text, in the same order.
    /// Returns an empty `Vec` when `texts` is empty.
    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

/// Build an [`Embedder`] from a config.
///
/// For `"fastembed"` this loads the ONNX model from disk (or downloads it on
/// first use), which can take several seconds.  For REST providers the
/// returned value is lightweight.
pub fn build_embedder(config: &EmbedderConfig) -> Result<Box<dyn Embedder + Send + Sync>> {
    match config.provider.as_str() {
        "openai" | "ollama" => Ok(Box::new(RestEmbedder {
            config: config.clone(),
        })),
        #[cfg(feature = "fastembed-embed")]
        "fastembed" => Ok(Box::new(FastEmbedEmbedder::new(config)?)),
        other => Err(Error::Embed(format!(
            "unknown embedding provider `{other}`; supported values: openai, ollama{}",
            if cfg!(feature = "fastembed-embed") {
                ", fastembed"
            } else {
                ""
            }
        ))),
    }
}

// ── REST embedder (OpenAI / Ollama) ───────────────────────────────────────────

struct RestEmbedder {
    config: EmbedderConfig,
}

impl Embedder for RestEmbedder {
    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        match self.config.provider.as_str() {
            "openai" => embed_openai(&self.config, texts),
            "ollama" => embed_ollama(&self.config, texts),
            other => Err(Error::Embed(format!("unknown REST provider `{other}`"))),
        }
    }
}

// ── fastembed embedder ────────────────────────────────────────────────────────

#[cfg(feature = "fastembed-embed")]
struct FastEmbedEmbedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
}

#[cfg(feature = "fastembed-embed")]
impl FastEmbedEmbedder {
    fn new(config: &EmbedderConfig) -> Result<Self> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

        let model_variant = match config.model.as_str() {
            "AllMiniLML6V2" => EmbeddingModel::AllMiniLML6V2,
            "BGESmallENV15" => EmbeddingModel::BGESmallENV15,
            "BGEBaseENV15" => EmbeddingModel::BGEBaseENV15,
            "BGELargeENV15" => EmbeddingModel::BGELargeENV15,
            "NomicEmbedTextV1" => EmbeddingModel::NomicEmbedTextV1,
            "NomicEmbedTextV15" => EmbeddingModel::NomicEmbedTextV15,
            "MultilingualE5Small" => EmbeddingModel::MultilingualE5Small,
            "MultilingualE5Base" => EmbeddingModel::MultilingualE5Base,
            "MultilingualE5Large" => EmbeddingModel::MultilingualE5Large,
            other => {
                return Err(Error::Embed(format!(
                    "unknown fastembed model `{other}`; \
                     supported: AllMiniLML6V2, BGESmallENV15, BGEBaseENV15, BGELargeENV15, \
                     NomicEmbedTextV1, NomicEmbedTextV15, \
                     MultilingualE5Small, MultilingualE5Base, MultilingualE5Large"
                )));
            }
        };

        let cache_dir = config
            .cache_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("fastembed"));
        let model = TextEmbedding::try_new(
            InitOptions::new(model_variant)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true),
        )
        .map_err(|e| Error::Embed(format!("failed to load fastembed model: {e}")))?;

        Ok(Self {
            model: std::sync::Mutex::new(model),
        })
    }
}

#[cfg(feature = "fastembed-embed")]
impl Embedder for FastEmbedEmbedder {
    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let texts_owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        self.model
            .lock()
            .unwrap()
            .embed(texts_owned, None)
            .map_err(|e| Error::Embed(format!("fastembed embedding failed: {e}")))
    }
}

// ── OpenAI-compatible ─────────────────────────────────────────────────────────

fn embed_openai(config: &EmbedderConfig, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let api_key_env = config.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
    let api_key = std::env::var(api_key_env)
        .map_err(|_| Error::Embed(format!("environment variable `{api_key_env}` is not set")))?;

    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or("https://api.openai.com");
    let url = format!("{base_url}/v1/embeddings");

    let body = serde_json::json!({
        "model": config.model,
        "input": texts,
    });

    let response: serde_json::Value = ureq::post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| Error::Embed(e.to_string()))?
        .into_body()
        .read_json()
        .map_err(|e| Error::Embed(e.to_string()))?;

    parse_openai_response(&response, texts.len())
}

fn parse_openai_response(response: &serde_json::Value, expected: usize) -> Result<Vec<Vec<f32>>> {
    let data = response["data"]
        .as_array()
        .ok_or_else(|| Error::Embed("unexpected OpenAI response: missing `data` array".into()))?;

    let mut results = vec![Vec::new(); expected];
    for item in data {
        let index = item["index"]
            .as_u64()
            .ok_or_else(|| Error::Embed("missing `index` in embedding object".into()))?
            as usize;
        let vec = parse_float_array(&item["embedding"])?;
        if index < results.len() {
            results[index] = vec;
        }
    }
    Ok(results)
}

// ── Ollama ────────────────────────────────────────────────────────────────────

fn embed_ollama(config: &EmbedderConfig, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    let url = format!("{base_url}/api/embed");

    let body = serde_json::json!({
        "model": config.model,
        "input": texts,
    });

    let response: serde_json::Value = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| Error::Embed(e.to_string()))?
        .into_body()
        .read_json()
        .map_err(|e| Error::Embed(e.to_string()))?;

    response["embeddings"]
        .as_array()
        .ok_or_else(|| {
            Error::Embed("unexpected Ollama response: missing `embeddings` array".into())
        })?
        .iter()
        .map(parse_float_array)
        .collect()
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_float_array(value: &serde_json::Value) -> Result<Vec<f32>> {
    value
        .as_array()
        .ok_or_else(|| Error::Embed("embedding value is not a JSON array".into()))?
        .iter()
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| Error::Embed("non-numeric value in embedding vector".into()))
        })
        .collect()
}
