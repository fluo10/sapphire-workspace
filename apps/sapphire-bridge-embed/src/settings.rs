//! `<bridge dir>/embedding.toml`: per-device embedding settings.
//!
//! ```toml
//! enabled   = true
//! provider  = "local"            # "local" | "openai"
//! model     = "Qwen/Qwen3-VL-Embedding-2B"
//! dimension = 1024               # output dimension after MRL truncation
//! max_tokens = 1024              # content tokens kept (local only)
//! # openai only:
//! endpoint    = "https://api.openai.com"
//! api_key_env = "OPENAI_API_KEY"
//! ```

use std::path::Path;

use crate::{Error, Result};

/// The local model: the only one supported for now.
pub const LOCAL_MODEL: &str = "Qwen/Qwen3-VL-Embedding-2B";

/// Which provider computes the vectors.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Qwen3-VL-Embedding-2B on this host's CPU.
    Local,
    /// An OpenAI-compatible `/v1/embeddings` endpoint.
    Openai,
}

/// The contents of `embedding.toml`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EmbeddingSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_provider")]
    pub provider: Provider,
    #[serde(default = "default_model")]
    pub model: String,
    /// Output dimension. Local vectors are cut to it (MRL) and normalized again.
    #[serde(default = "default_dimension")]
    pub dimension: u32,
    /// Content tokens kept before templating (local only).
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Base URL of the OpenAI-compatible endpoint (openai only).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Environment variable holding the API key (openai only; default `OPENAI_API_KEY`).
    #[serde(default)]
    pub api_key_env: Option<String>,
}

fn default_provider() -> Provider {
    Provider::Local
}
fn default_model() -> String {
    LOCAL_MODEL.to_owned()
}
fn default_dimension() -> u32 {
    1024
}
fn default_max_tokens() -> usize {
    1024
}

impl EmbeddingSettings {
    /// The file name inside the bridge directory.
    pub const FILE: &'static str = "embedding.toml";

    /// Read and validate `path`. `Ok(None)` when the file is absent.
    ///
    /// For the local provider, `dimension` must be in `64..=2048` and `model` must be
    /// [`LOCAL_MODEL`]. `dimension` and (for local) `max_tokens` must not be zero.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let body = match std::fs::read_to_string(path) {
            Ok(body) => body,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Settings(format!("{}: {e}", path.display()))),
        };
        let settings: Self = toml::from_str(&body)
            .map_err(|e| Error::Settings(format!("{}: {e}", path.display())))?;
        settings
            .validate()
            .map_err(|e| Error::Settings(format!("{}: {e}", path.display())))?;
        Ok(Some(settings))
    }

    fn validate(&self) -> std::result::Result<(), String> {
        if self.dimension == 0 {
            return Err("`dimension` must be at least 1".into());
        }
        if self.provider == Provider::Local {
            if !(64..=2048).contains(&self.dimension) {
                return Err(format!(
                    "`dimension` = {} is out of range for the local model (64..=2048)",
                    self.dimension
                ));
            }
            if self.max_tokens == 0 {
                return Err("`max_tokens` must be at least 1".into());
            }
            if self.model != LOCAL_MODEL {
                return Err(format!(
                    "the local provider supports only `{LOCAL_MODEL}`, not `{}`",
                    self.model
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(EmbeddingSettings::FILE);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let got = EmbeddingSettings::load(&dir.path().join(EmbeddingSettings::FILE)).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn minimal_file_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let got = EmbeddingSettings::load(&write(&dir, "enabled = true\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            got,
            EmbeddingSettings {
                enabled: true,
                provider: Provider::Local,
                model: "Qwen/Qwen3-VL-Embedding-2B".into(),
                dimension: 1024,
                max_tokens: 1024,
                endpoint: None,
                api_key_env: None,
            }
        );
    }

    #[test]
    fn local_dimension_over_2048_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = EmbeddingSettings::load(&write(&dir, "enabled = true\ndimension = 4096\n"))
            .unwrap_err();
        assert!(matches!(err, Error::Settings(_)), "{err}");
    }

    #[test]
    fn openai_dimension_is_not_capped() {
        let dir = tempfile::tempdir().unwrap();
        let got = EmbeddingSettings::load(&write(
            &dir,
            "enabled = true\nprovider = \"openai\"\nmodel = \"text-embedding-3-large\"\ndimension = 3072\n",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(got.provider, Provider::Openai);
        assert_eq!(got.dimension, 3072);
    }

    #[test]
    fn invalid_toml_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = EmbeddingSettings::load(&write(&dir, "enabled = \"maybe\"\n")).unwrap_err();
        assert!(matches!(err, Error::Settings(_)), "{err}");
    }
}
