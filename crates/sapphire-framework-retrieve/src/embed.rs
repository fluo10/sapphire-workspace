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
        let send: fn(&EmbedderConfig, &[&str]) -> Result<Vec<Vec<f32>>> =
            match self.config.provider.as_str() {
                "openai" => embed_openai,
                "ollama" => embed_ollama,
                other => return Err(Error::Embed(format!("unknown REST provider `{other}`"))),
            };
        let capped: Vec<&str> = texts
            .iter()
            .map(|t| cap_chars(t, MAX_REST_EMBED_CHARS))
            .collect();

        // One sub-request per group, results concatenated in input order.
        let mut out = Vec::with_capacity(capped.len());
        for group in request_groups(&capped, MAX_REST_REQUEST_CHARS) {
            let inputs = &capped[group];
            let vectors = send(&self.config, inputs)?;
            if vectors.len() != inputs.len() {
                return Err(Error::Embed(format!(
                    "embedding provider returned {} vectors for {} inputs",
                    vectors.len(),
                    inputs.len()
                )));
            }
            out.extend(vectors);
        }
        Ok(out)
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

    // `texts` arrive capped and grouped from `RestEmbedder::embed_texts`.
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

    // `texts` arrive capped and grouped from `RestEmbedder::embed_texts`.
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

// ── REST input cap ────────────────────────────────────────────────────────────

/// Input cap for REST embedders, in characters.
///
/// OpenAI's embedding models reject an input over 8,191 tokens. In cl100k a
/// Japanese kanji is often 2–3 tokens, so 4,000 characters keeps even dense CJK
/// text under that limit with some margin. This is a safety net only: #185
/// replaces it with token-level truncation.
pub(crate) const MAX_REST_EMBED_CHARS: usize = 4_000;

/// Upper bound on the total characters sent in one REST request.
///
/// A batch is split into sub-requests of at most this many characters (after
/// [`MAX_REST_EMBED_CHARS`] is applied), so a batch of large files does not
/// become one oversized request body.
pub(crate) const MAX_REST_REQUEST_CHARS: usize = 200_000;

/// Split `texts` into consecutive groups whose total length, in characters,
/// is at most `limit`. Order is kept, every input is in exactly one group, and
/// an input longer than `limit` gets a group of its own.
fn request_groups(texts: &[&str], limit: usize) -> Vec<std::ops::Range<usize>> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut total = 0;
    for (i, text) in texts.iter().enumerate() {
        let len = text.chars().count();
        if i > start && total + len > limit {
            groups.push(start..i);
            start = i;
            total = 0;
        }
        total += len;
    }
    if start < texts.len() {
        groups.push(start..texts.len());
    }
    groups
}

/// `text` cut to at most `max` characters, on a `char` boundary.
pub(crate) fn cap_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    #[test]
    fn cap_keeps_short_input() {
        assert_eq!(cap_chars("abc", 8), "abc");
    }

    #[test]
    fn cap_cuts_on_a_char_boundary() {
        let s = "日本語のテキスト";
        assert_eq!(cap_chars(s, 3), "日本語");
    }
}

#[cfg(test)]
mod rest_split_tests {
    use super::*;

    #[test]
    fn the_cap_is_four_thousand_chars() {
        assert_eq!(MAX_REST_EMBED_CHARS, 4_000);
        let kanji = "漢".repeat(5_000);
        assert_eq!(
            cap_chars(&kanji, MAX_REST_EMBED_CHARS).chars().count(),
            4_000
        );
    }

    #[test]
    fn groups_keep_order_and_stay_within_the_limit() {
        assert_eq!(
            request_groups(&["aa", "bbb", "c", "dddd", "e"], 5),
            vec![0..2, 2..4, 4..5]
        );
    }

    #[test]
    fn an_input_over_the_limit_gets_a_group_of_its_own() {
        assert_eq!(
            request_groups(&["a", "abcdef", "b"], 5),
            vec![0..1, 1..2, 2..3]
        );
    }

    #[test]
    fn no_inputs_means_no_groups() {
        assert!(request_groups(&[], 5).is_empty());
    }

    #[test]
    fn chars_not_bytes_are_counted() {
        // Three kanji are 9 bytes but 3 chars.
        assert_eq!(request_groups(&["日本語", "テキ"], 5), vec![0..2]);
    }
}
