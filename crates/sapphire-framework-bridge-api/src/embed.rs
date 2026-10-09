//! Embedding settings: what the `embed.*` settings methods carry.
//!
//! A workgroup configures at most one *local* model and at most one *remote* one; each
//! device switches each slot on or off, and keeps the remote slot's API key to itself.
//! See `docs/superpowers/specs/2026-10-10-embedding-settings-design.md`.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::EmbedInfoResult;

/// The only local model for now.
pub const LOCAL_MODEL: &str = "Qwen/Qwen3-VL-Embedding-2B";
/// Upper bound on [`LocalModel::max_tokens`].
pub const MAX_TOKENS: usize = 8192;
/// [`LocalModel::dimension`]'s default, and the dimension #183 settled on.
pub const DEFAULT_DIMENSION: u32 = 1024;
/// [`LocalModel::max_tokens`]'s default.
pub const DEFAULT_MAX_TOKENS: usize = 1024;

/// One of the two model slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    /// Computed on this device's CPU.
    Local,
    /// An OpenAI-compatible `/v1/embeddings` endpoint.
    Remote,
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Slot::Local => "local",
            Slot::Remote => "remote",
        })
    }
}

/// The local slot's model.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModel {
    /// The model's name; only [`LOCAL_MODEL`] is supported.
    #[serde(default = "default_local_model")]
    pub model: String,
    /// Output dimension; vectors are cut to it (MRL) and normalized again.
    #[serde(default = "default_dimension")]
    pub dimension: u32,
    /// Content tokens kept before the template is applied.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
}

fn default_local_model() -> String {
    LOCAL_MODEL.to_owned()
}
fn default_dimension() -> u32 {
    DEFAULT_DIMENSION
}
fn default_max_tokens() -> usize {
    DEFAULT_MAX_TOKENS
}

impl Default for LocalModel {
    fn default() -> Self {
        LocalModel {
            model: default_local_model(),
            dimension: DEFAULT_DIMENSION,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

/// The remote slot's model.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteModel {
    /// The endpoint's base URL; requests go to `<endpoint>/v1/embeddings`.
    pub endpoint: String,
    /// The model's name, as the endpoint knows it.
    pub model: String,
    /// Output dimension; longer vectors are cut to it (MRL) and normalized again.
    pub dimension: u32,
}

/// The two slots: the workgroup's synced `embedding.toml`, or a host's own without one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    /// The local slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalModel>,
    /// The remote slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteModel>,
}

impl ModelSettings {
    /// Neither slot is configured.
    pub fn is_empty(&self) -> bool {
        self.local.is_none() && self.remote.is_none()
    }

    /// Check both slots; the message names the field at fault.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(local) = &self.local {
            if local.model != LOCAL_MODEL {
                return Err(format!(
                    "local: only `{LOCAL_MODEL}` is supported, not `{}`",
                    local.model
                ));
            }
            if !(64..=2048).contains(&local.dimension) {
                return Err(format!(
                    "local: `dimension` = {} is out of range (64..=2048)",
                    local.dimension
                ));
            }
            if !(1..=MAX_TOKENS).contains(&local.max_tokens) {
                return Err(format!(
                    "local: `max_tokens` = {} is out of range (1..={MAX_TOKENS})",
                    local.max_tokens
                ));
            }
        }
        if let Some(remote) = &self.remote {
            if !(remote.endpoint.starts_with("http://") || remote.endpoint.starts_with("https://"))
            {
                return Err(format!(
                    "remote: `endpoint` = `{}` is not an http(s) URL",
                    remote.endpoint
                ));
            }
            if remote.model.trim().is_empty() {
                return Err("remote: `model` must not be empty".to_owned());
            }
            if remote.dimension == 0 {
                return Err("remote: `dimension` must be at least 1".to_owned());
            }
        }
        Ok(())
    }
}

/// One slot's model, for [`EmbedModelSetParams`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotModel {
    /// The local slot's model.
    Local(LocalModel),
    /// The remote slot's model.
    Remote(RemoteModel),
}

impl SlotModel {
    /// The slot this model belongs in.
    pub fn slot(&self) -> Slot {
        match self {
            SlotModel::Local(_) => Slot::Local,
            SlotModel::Remote(_) => Slot::Remote,
        }
    }
}

/// This device's own settings, never synced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct DeviceSettings {
    /// The local slot's switch; `None` is auto (off without AVX2).
    #[serde(default)]
    pub local_enabled: Option<bool>,
    /// The remote slot's switch; `None` is auto (on).
    #[serde(default)]
    pub remote_enabled: Option<bool>,
    /// Where the local model's files are cached, when not the bridge's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
}

impl DeviceSettings {
    /// One slot's switch.
    pub fn enabled(&self, slot: Slot) -> Option<bool> {
        match slot {
            Slot::Local => self.local_enabled,
            Slot::Remote => self.remote_enabled,
        }
    }
}

/// The remote slot's API key. Never printed: `Debug` shows `ApiKey(..)`.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key; surrounding whitespace (a trailing newline from a pipe) is dropped.
    pub fn new(key: impl Into<String>) -> ApiKey {
        ApiKey(key.into().trim().to_owned())
    }

    /// The key itself, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the key is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(..)")
    }
}

/// Where the model settings in effect come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    /// The workgroup's synced `embedding.toml`.
    Workgroup,
    /// This host's own `[models]`, because it has no workgroup or the workgroup has none.
    Device,
}

/// Why a bridge does not embed, or what it lacks while it does.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedNote {
    /// No slot is configured.
    NotConfigured,
    /// Every configured slot is switched off on this device.
    DisabledOnDevice,
    /// Only the local slot is configured, and auto turned it off: no AVX2.
    NoAvx2,
    /// The remote slot is active without an API key (the call is still attempted).
    KeyMissing,
    /// The settings could not be read or are invalid.
    Invalid(String),
}

impl fmt::Display for EmbedNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbedNote::NotConfigured => f.write_str("no model is configured"),
            EmbedNote::DisabledOnDevice => f.write_str("turned off on this device"),
            EmbedNote::NoAvx2 => f.write_str("the local model needs AVX2, which this CPU lacks"),
            EmbedNote::KeyMissing => f.write_str("no API key is set for the remote model"),
            EmbedNote::Invalid(why) => write!(f, "invalid settings: {why}"),
        }
    }
}

/// Result of every `embed.*` settings method: the settings, and what they resolve to.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EmbedSettingsResult {
    /// The model settings in effect.
    pub models: ModelSettings,
    /// Where they come from; `None` when there are none.
    #[serde(default)]
    pub source: Option<ModelSource>,
    /// This host's own `[models]` exists but the workgroup's file wins.
    #[serde(default)]
    pub shadowed_device_models: bool,
    /// This device's own settings, as written.
    #[serde(default)]
    pub device: DeviceSettings,
    /// The local slot's effective switch.
    #[serde(default)]
    pub local_enabled: bool,
    /// The remote slot's effective switch.
    #[serde(default)]
    pub remote_enabled: bool,
    /// The slot the bridge serves.
    #[serde(default)]
    pub active: Option<Slot>,
    /// Whether this CPU has AVX2 (always true off x86_64).
    #[serde(default)]
    pub avx2: bool,
    /// Whether an API key is stored on this device.
    #[serde(default)]
    pub key_set: bool,
    /// What `embed.info` answers now.
    #[serde(default)]
    pub info: EmbedInfoResult,
}

impl EmbedSettingsResult {
    /// One slot's effective switch.
    pub fn enabled(&self, slot: Slot) -> bool {
        match slot {
            Slot::Local => self.local_enabled,
            Slot::Remote => self.remote_enabled,
        }
    }
}

/// Parameters of [`EMBED_MODEL_SET`](crate::EMBED_MODEL_SET).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmbedModelSetParams {
    /// The slot to set or clear.
    pub slot: Slot,
    /// Its new model; `None` clears the slot. Must belong to `slot`.
    #[serde(default)]
    pub model: Option<SlotModel>,
}

/// Parameters of [`EMBED_DEVICE_SET`](crate::EMBED_DEVICE_SET).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmbedDeviceSetParams {
    /// The slot to switch.
    pub slot: Slot,
    /// `None` is auto.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Parameters of [`EMBED_KEY_SET`](crate::EMBED_KEY_SET). `Debug` hides the key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmbedKeySetParams {
    /// The key.
    pub key: ApiKey,
}

/// One settings request, as either CLI builds it from its arguments.
///
/// The bridge's CLI carries it out on the files when no bridge is running; both CLIs send
/// it to a running bridge with [`BridgeClient::embed_request`](crate::BridgeClient::embed_request).
#[derive(Clone, Debug)]
pub enum EmbedRequest {
    /// Read the settings.
    Show,
    /// Set or clear a slot's model.
    ModelSet(EmbedModelSetParams),
    /// Switch a slot on this device.
    DeviceSet(EmbedDeviceSetParams),
    /// Store the API key.
    KeySet(ApiKey),
    /// Remove the API key.
    KeyClear,
}

/// What `embedding show` prints, one line each.
pub fn describe(s: &EmbedSettingsResult) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(match s.source {
        Some(ModelSource::Workgroup) if s.shadowed_device_models => {
            "models: the workgroup's (this device's own [models] is ignored)".to_owned()
        }
        Some(ModelSource::Workgroup) => "models: the workgroup's".to_owned(),
        Some(ModelSource::Device) => "models: this device's own".to_owned(),
        None => "models: none".to_owned(),
    });
    let switch = |slot: Slot| -> String {
        let on = if s.enabled(slot) { "on" } else { "off" };
        match s.device.enabled(slot) {
            Some(_) => on.to_owned(),
            None if slot == Slot::Local && !s.avx2 => format!("{on} (auto: no AVX2)"),
            None => format!("{on} (auto)"),
        }
    };
    lines.push(match &s.models.local {
        Some(m) => format!(
            "local:  {} ({} dimensions, {} tokens) — this device: {}",
            m.model,
            m.dimension,
            m.max_tokens,
            switch(Slot::Local)
        ),
        None => "local:  not configured".to_owned(),
    });
    lines.push(match &s.models.remote {
        Some(m) => format!(
            "remote: {} at {} ({} dimensions) — this device: {}, key {}",
            m.model,
            m.endpoint,
            m.dimension,
            switch(Slot::Remote),
            if s.key_set { "set" } else { "not set" }
        ),
        None => format!(
            "remote: not configured{}",
            if s.key_set { " (a key is set)" } else { "" }
        ),
    });
    let state = match (&s.active, &s.info.note) {
        (Some(slot), None) => format!(
            "active: {slot}, {}",
            if s.info.loaded {
                "loaded"
            } else {
                "not loaded"
            }
        ),
        (Some(slot), Some(note)) => format!("active: {slot} — {note}"),
        (None, Some(note)) => format!("active: none — {note}"),
        (None, None) => "active: none".to_owned(),
    };
    lines.push(state);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> RemoteModel {
        RemoteModel {
            endpoint: "https://api.example.com".into(),
            model: "m".into(),
            dimension: 8,
        }
    }

    #[test]
    fn local_defaults_validate() {
        let s = ModelSettings {
            local: Some(LocalModel::default()),
            remote: Some(remote()),
        };
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn validation_keeps_the_local_rules() {
        let mut local = LocalModel {
            dimension: 4096,
            ..LocalModel::default()
        };
        let check = |l: &LocalModel| {
            ModelSettings {
                local: Some(l.clone()),
                remote: None,
            }
            .validate()
        };
        assert!(check(&local).is_err());
        local.dimension = 1024;
        local.max_tokens = MAX_TOKENS + 1;
        assert!(check(&local).is_err());
        local.max_tokens = MAX_TOKENS;
        assert!(check(&local).is_ok());
        local.model = "other".into();
        assert!(check(&local).is_err());
    }

    #[test]
    fn validation_checks_the_remote_slot() {
        let check = |r: RemoteModel| {
            ModelSettings {
                local: None,
                remote: Some(r),
            }
            .validate()
        };
        assert!(
            check(RemoteModel {
                endpoint: "api.example.com".into(),
                ..remote()
            })
            .is_err()
        );
        assert!(
            check(RemoteModel {
                model: " ".into(),
                ..remote()
            })
            .is_err()
        );
        assert!(
            check(RemoteModel {
                dimension: 0,
                ..remote()
            })
            .is_err()
        );
    }

    #[test]
    fn the_key_never_prints() {
        let key = ApiKey::new("sk-secret\n");
        assert_eq!(key.expose(), "sk-secret");
        let params = EmbedKeySetParams { key };
        let shown = format!("{params:?} {:?}", EmbedRequest::KeySet(params.key.clone()));
        assert!(!shown.contains("sk-secret"), "{shown}");
        // The wire form carries it, plainly.
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            serde_json::json!({ "key": "sk-secret" })
        );
    }

    #[test]
    fn slot_models_are_tagged_by_slot() {
        let v = serde_json::to_value(SlotModel::Remote(remote())).unwrap();
        assert_eq!(v["remote"]["model"], "m");
        let back: SlotModel = serde_json::from_value(v).unwrap();
        assert_eq!(back.slot(), Slot::Remote);
    }

    #[test]
    fn an_info_without_a_note_still_parses() {
        let info: EmbedInfoResult =
            serde_json::from_value(serde_json::json!({ "enabled": false })).unwrap();
        assert_eq!(info.note, None);
    }

    #[test]
    fn describe_names_the_source_switches_and_state() {
        let s = EmbedSettingsResult {
            models: ModelSettings {
                local: Some(LocalModel::default()),
                remote: Some(remote()),
            },
            source: Some(ModelSource::Workgroup),
            local_enabled: false,
            remote_enabled: true,
            active: Some(Slot::Remote),
            avx2: false,
            key_set: false,
            info: EmbedInfoResult {
                enabled: true,
                note: Some(EmbedNote::KeyMissing),
                ..EmbedInfoResult::default()
            },
            ..EmbedSettingsResult::default()
        };
        let lines = describe(&s);
        assert_eq!(lines[0], "models: the workgroup's");
        assert!(
            lines[1].ends_with("this device: off (auto: no AVX2)"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].ends_with("this device: on (auto), key not set"),
            "{}",
            lines[2]
        );
        assert_eq!(
            lines[3],
            "active: remote — no API key is set for the remote model"
        );
    }

    #[test]
    fn describe_without_settings() {
        let s = EmbedSettingsResult {
            avx2: true,
            info: EmbedInfoResult {
                note: Some(EmbedNote::NotConfigured),
                ..EmbedInfoResult::default()
            },
            ..EmbedSettingsResult::default()
        };
        assert_eq!(
            describe(&s),
            vec![
                "models: none",
                "local:  not configured",
                "remote: not configured",
                "active: none — no model is configured",
            ]
        );
    }
}
