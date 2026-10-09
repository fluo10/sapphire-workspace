//! An OpenAI-compatible `/v1/embeddings` provider, with the #194 fixes.

use std::ops::Range;
use std::time::Duration;

use crate::service::Embed;
use crate::settings::EmbeddingSettings;
use crate::template::mrl;
use crate::{Error, Result};

/// Input cap, in characters, applied to every input before it is sent.
///
/// A character cap stays until a tokenizer for the remote model is known. OpenAI's embedding
/// models reject an input over 8,191 tokens. Japanese averages about 1–1.5 cl100k tokens per
/// character, so 4,000 characters stays under that limit for typical text. Pathological text
/// (dense rare kanji, long runs of symbols) can still exceed it; such an input then fails on
/// its own, and the caller's per-item retry isolates it.
pub const MAX_INPUT_CHARS: usize = 4_000;

/// Upper bound on the characters sent in one request, summed over its (capped) inputs.
pub const MAX_REQUEST_CHARS: usize = 100_000;

/// The default endpoint when `endpoint` is not set.
pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com";

/// The default environment variable for the API key.
pub const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// `text` cut to at most `max` characters, on a `char` boundary.
pub fn cap_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

/// Group input indices, in order, so each group's characters total at most
/// [`MAX_REQUEST_CHARS`]. Lengths are measured as given, so callers pass inputs already
/// capped with [`cap_chars`]. An input over the cap alone forms its own group. Empty input
/// gives no groups.
pub fn request_groups(texts: &[&str]) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut total = 0;
    for (i, text) in texts.iter().enumerate() {
        let len = text.chars().count();
        if i > start && total + len > MAX_REQUEST_CHARS {
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

fn parse_response(response: &serde_json::Value, expected: usize) -> Result<Vec<Vec<f32>>> {
    let data = response["data"]
        .as_array()
        .ok_or_else(|| Error::Embed("unexpected response: missing `data` array".into()))?;
    if data.len() != expected {
        return Err(Error::Embed(format!(
            "the endpoint returned {} vectors for {expected} inputs",
            data.len()
        )));
    }
    let mut out: Vec<Option<Vec<f32>>> = vec![None; expected];
    for item in data {
        let index = item["index"]
            .as_u64()
            .ok_or_else(|| Error::Embed("missing `index` in an embedding object".into()))?
            as usize;
        let slot = out
            .get_mut(index)
            .ok_or_else(|| Error::Embed(format!("`index` {index} out of range")))?;
        if slot.is_some() {
            return Err(Error::Embed(format!("`index` {index} returned twice")));
        }
        let vector = item["embedding"]
            .as_array()
            .ok_or_else(|| Error::Embed("`embedding` is not an array".into()))?
            .iter()
            .map(|v| {
                v.as_f64()
                    .map(|f| f as f32)
                    .ok_or_else(|| Error::Embed("non-numeric value in an embedding".into()))
            })
            .collect::<Result<Vec<f32>>>()?;
        *slot = Some(vector);
    }
    // Every slot is filled: `expected` distinct, in-range indices.
    Ok(out.into_iter().map(Option::unwrap_or_default).collect())
}

/// One JSON POST. A trait so tests can stand in for the network.
pub trait HttpPost: Send + Sync {
    fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> Result<serde_json::Value>;
}

/// Whole-request timeout for REST calls. ureq has none by default, and a stalled endpoint
/// would otherwise block the single embedding worker forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How much of an error response's body goes into the error message, in characters.
const ERROR_BODY_CHARS: usize = 500;

/// [`HttpPost`] over one shared `ureq::Agent` (blocking; runs on the embedding worker
/// thread), with [`REQUEST_TIMEOUT`].
#[derive(Clone)]
pub struct UreqPost {
    agent: ureq::Agent,
}

impl Default for UreqPost {
    fn default() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            // Non-2xx responses come back as responses, so their body can be reported.
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }
}

impl HttpPost for UreqPost {
    fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut request = self
            .agent
            .post(url)
            .header("Content-Type", "application/json");
        if let Some(key) = bearer {
            request = request.header("Authorization", &format!("Bearer {key}"));
        }
        let response = request
            .send_json(body)
            .map_err(|e| Error::Embed(format!("POST {url}: {e}")))?;
        let status = response.status();
        let mut body = response.into_body();
        if !status.is_success() {
            let text = body.read_to_string().unwrap_or_default();
            return Err(Error::Embed(format!(
                "POST {url}: HTTP {status}: {}",
                cap_chars(text.trim(), ERROR_BODY_CHARS)
            )));
        }
        body.read_json()
            .map_err(|e| Error::Embed(format!("POST {url}: {e}")))
    }
}

/// Embeds through an OpenAI-compatible endpoint.
pub struct RestEmbedder<H: HttpPost = UreqPost> {
    http: H,
    url: String,
    model: String,
    dimension: usize,
    api_key_env: String,
}

impl RestEmbedder<UreqPost> {
    /// A provider for `settings`, over `ureq`.
    pub fn new(settings: &EmbeddingSettings) -> Self {
        Self::with_http(settings, UreqPost::default())
    }
}

impl<H: HttpPost> RestEmbedder<H> {
    /// A provider for `settings` over `http`.
    pub fn with_http(settings: &EmbeddingSettings, http: H) -> Self {
        let endpoint = settings
            .endpoint
            .as_deref()
            .unwrap_or(DEFAULT_ENDPOINT)
            .trim_end_matches('/');
        Self {
            http,
            url: format!("{endpoint}/v1/embeddings"),
            model: settings.model.clone(),
            dimension: settings.dimension as usize,
            api_key_env: settings
                .api_key_env
                .clone()
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned()),
        }
    }

    /// Embed `texts`, one request per [`request_groups`] group, results in input order.
    ///
    /// The key is read from `api_key_env` on every call; when the variable is unset, no
    /// `Authorization` header is sent (keyless local endpoints). Vectors longer than
    /// `dimension` are cut and normalized again (MRL), vectors of exactly `dimension` are
    /// used as returned, and a shorter vector is an error.
    pub fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let capped: Vec<&str> = texts
            .iter()
            .map(|t| cap_chars(t, MAX_INPUT_CHARS))
            .collect();
        let key = std::env::var(&self.api_key_env)
            .ok()
            .filter(|k| !k.is_empty());
        let mut out = Vec::with_capacity(capped.len());
        for group in request_groups(&capped) {
            let inputs = &capped[group];
            let body = serde_json::json!({ "model": self.model, "input": inputs });
            let response = self.http.post_json(&self.url, key.as_deref(), body)?;
            for v in parse_response(&response, inputs.len())? {
                if v.len() < self.dimension {
                    return Err(Error::Embed(format!(
                        "the endpoint returned a {}-dimension vector, shorter than the \
                         configured dimension {}",
                        v.len(),
                        self.dimension
                    )));
                }
                out.push(if v.len() > self.dimension {
                    mrl(&v, self.dimension)
                } else {
                    v
                });
            }
        }
        Ok(out)
    }
}

impl<H: HttpPost + 'static> Embed for RestEmbedder<H> {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_texts(texts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn settings(dimension: u32) -> EmbeddingSettings {
        EmbeddingSettings {
            enabled: true,
            provider: crate::Provider::Openai,
            model: "m".into(),
            dimension,
            max_tokens: 1024,
            endpoint: Some("http://example.invalid/".into()),
            api_key_env: Some("SAPPHIRE_BRIDGE_EMBED_TEST_KEY_UNSET".into()),
            cache_dir: None,
        }
    }

    /// Answers each request with `dim`-length vectors whose first value is the input's
    /// length, listed in reverse `index` order. `drop_one` returns one vector too few.
    struct Fake {
        dim: usize,
        drop_one: bool,
        calls: Mutex<Vec<(String, usize)>>,
    }

    impl Fake {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                drop_one: false,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpPost for Fake {
        fn post_json(
            &self,
            url: &str,
            _bearer: Option<&str>,
            body: serde_json::Value,
        ) -> Result<serde_json::Value> {
            assert_eq!(body["model"], "m");
            let inputs = body["input"].as_array().unwrap();
            self.calls
                .lock()
                .unwrap()
                .push((url.to_owned(), inputs.len()));
            let mut data: Vec<serde_json::Value> = inputs
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let mut v = vec![0.5f32; self.dim];
                    v[0] = s.as_str().unwrap().chars().count() as f32;
                    serde_json::json!({ "index": i, "embedding": v })
                })
                .collect();
            data.reverse();
            if self.drop_one {
                data.pop();
            }
            Ok(serde_json::json!({ "data": data }))
        }
    }

    #[test]
    fn cap_chars_cuts_on_a_char_boundary() {
        assert_eq!(cap_chars("今日は雨", 2), "今日");
        assert_eq!(cap_chars("abc", 10), "abc");
    }

    #[test]
    fn request_groups_keep_order() {
        let big = "x".repeat(MAX_INPUT_CHARS);
        let texts: Vec<&str> = (0..30).map(|_| big.as_str()).collect();
        // 25 × 4,000 = 100,000 fits exactly; the rest go in the next group.
        assert_eq!(request_groups(&texts), vec![0..25, 25..30]);
    }

    #[test]
    fn request_groups_put_an_over_cap_input_alone() {
        let huge = "x".repeat(MAX_REQUEST_CHARS + 1);
        let texts = ["a", huge.as_str(), "b"];
        assert_eq!(request_groups(&texts), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn request_groups_of_nothing_is_nothing() {
        assert!(request_groups(&[]).is_empty());
    }

    #[test]
    fn results_follow_index_not_response_order() {
        let e = RestEmbedder::with_http(&settings(4), Fake::new(4));
        let texts: Vec<String> = ["a", "bb", "ccc"].iter().map(|s| s.to_string()).collect();
        let out = e.embed_texts(&texts).unwrap();
        let firsts: Vec<f32> = out.iter().map(|v| v[0]).collect();
        assert_eq!(firsts, vec![1.0, 2.0, 3.0]);
        let calls = e.http.calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![("http://example.invalid/v1/embeddings".to_owned(), 3)]
        );
    }

    #[test]
    fn inputs_are_capped_before_sending() {
        let e = RestEmbedder::with_http(&settings(4), Fake::new(4));
        let out = e.embed_texts(&["y".repeat(MAX_INPUT_CHARS + 50)]).unwrap();
        assert_eq!(out[0][0], MAX_INPUT_CHARS as f32);
    }

    #[test]
    fn a_count_mismatch_is_an_error() {
        let mut fake = Fake::new(4);
        fake.drop_one = true;
        let e = RestEmbedder::with_http(&settings(4), fake);
        let texts: Vec<String> = vec!["a".into(), "b".into()];
        assert!(matches!(e.embed_texts(&texts), Err(Error::Embed(_))));
    }

    #[test]
    fn longer_vectors_are_cut_to_dimension_and_normalized() {
        let e = RestEmbedder::with_http(&settings(1024), Fake::new(3000));
        let out = e.embed_texts(&["a".into(), "b".into()]).unwrap();
        for v in out {
            assert_eq!(v.len(), 1024);
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-5, "norm {n}");
        }
    }

    #[test]
    fn shorter_vectors_are_an_error() {
        let e = RestEmbedder::with_http(&settings(1024), Fake::new(768));
        let err = e.embed_texts(&["a".into()]).unwrap_err();
        assert!(matches!(err, Error::Embed(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("768") && msg.contains("1024"), "{msg}");
    }

    #[test]
    fn http_errors_carry_the_response_body() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            let body = r#"{"error":{"message":"Incorrect API key provided"}}"#;
            write!(
                reader.get_mut(),
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let err = UreqPost::default()
            .post_json(
                &format!("http://{addr}/v1/embeddings"),
                Some("k"),
                serde_json::json!({}),
            )
            .unwrap_err();
        server.join().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("Incorrect API key provided"), "{msg}");
    }

    #[test]
    fn empty_input_sends_nothing() {
        let e = RestEmbedder::with_http(&settings(4), Fake::new(4));
        assert!(e.embed_texts(&[]).unwrap().is_empty());
        assert!(e.http.calls.lock().unwrap().is_empty());
    }
}
