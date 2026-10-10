//! Where the embedding settings live, and what they resolve to (#186).
//!
//! Three places, by owner:
//!
//! - the workgroup's synced `root/embedding.toml` holds the two model slots;
//! - this host's `embedding.toml` holds its switch per slot, the local model's cache
//!   directory, and — only for a host without a workgroup's file — its own `[models]`;
//! - this host's `embedding.key` holds the remote slot's API key, readable by the owner only.
//!
//! [`load`] reads all three and [`resolve`]s them; the RPCs and the bridge's offline CLI
//! write them through the `set_*` functions.

use std::path::{Path, PathBuf};

use sapphire_bridge_api::{
    ApiKey, DeviceSettings, EmbedInfoResult, EmbedModelSetParams, EmbedNote, EmbedSettingsResult,
    LocalModel, ModelSettings, ModelSource, RemoteModel, Slot, SlotModel,
};
use serde::{Deserialize, Serialize};

use crate::dir::BridgeDir;
use crate::error::{Error, Result};
use crate::workgroup::Workgroup;

/// The settings file's name, in the bridge directory and in a workgroup's root alike.
pub const SETTINGS_FILE: &str = "embedding.toml";
/// The API key's file name, in the bridge directory.
pub const KEY_FILE: &str = "embedding.key";

/// This host's `embedding.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeviceFile {
    /// Where the local model's files are cached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_dir: Option<PathBuf>,
    /// The local slot's switch.
    #[serde(default, skip_serializing_if = "Switch::is_auto")]
    local: Switch,
    /// The remote slot's switch.
    #[serde(default, skip_serializing_if = "Switch::is_auto")]
    remote: Switch,
    /// The model settings of a host that has no workgroup's file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    models: Option<ModelSettings>,
}

/// One slot's switch in [`DeviceFile`]: `[local]` / `[remote]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Switch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

impl Switch {
    fn is_auto(&self) -> bool {
        self.enabled.is_none()
    }
}

impl DeviceFile {
    fn settings(&self) -> DeviceSettings {
        DeviceSettings {
            local_enabled: self.local.enabled,
            remote_enabled: self.remote.enabled,
            cache_dir: self.cache_dir.clone(),
        }
    }

    fn switch_mut(&mut self, slot: Slot) -> &mut Switch {
        match slot {
            Slot::Local => &mut self.local,
            Slot::Remote => &mut self.remote,
        }
    }
}

/// What the bridge serves, and with what: the input of the [`EmbedFactory`](crate::EmbedFactory).
///
/// Compared with the last one applied, so the provider is rebuilt only when this changes.
/// `Debug` never shows the key ([`ApiKey`] hides it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedConfig {
    /// The local slot.
    Local {
        /// Its model.
        model: LocalModel,
        /// Where its files are cached; `None` is the host's default.
        cache_dir: Option<PathBuf>,
    },
    /// The remote slot.
    Remote {
        /// Its model.
        model: RemoteModel,
        /// This device's key, if one is stored.
        key: Option<ApiKey>,
    },
}

/// The settings, read and resolved: everything [`EmbedSettingsResult`] reports but `info`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// The model settings in effect.
    pub models: ModelSettings,
    /// Where they come from.
    pub source: Option<ModelSource>,
    /// This host's own `[models]` is ignored because the workgroup's file wins.
    pub shadowed: bool,
    /// This host's switches and cache directory, as written.
    pub device: DeviceSettings,
    /// The local slot's effective switch.
    pub local_enabled: bool,
    /// The remote slot's effective switch.
    pub remote_enabled: bool,
    /// The slot served.
    pub active: Option<Slot>,
    /// Whether the CPU has AVX2.
    pub avx2: bool,
    /// Whether a key is stored.
    pub key_set: bool,
    /// Why embedding is off, or what it lacks.
    pub note: Option<EmbedNote>,
}

impl Resolved {
    /// The report, with what `embed.info` answers now.
    pub fn report(&self, info: EmbedInfoResult) -> EmbedSettingsResult {
        EmbedSettingsResult {
            models: self.models.clone(),
            source: self.source,
            shadowed_device_models: self.shadowed,
            device: self.device.clone(),
            local_enabled: self.local_enabled,
            remote_enabled: self.remote_enabled,
            active: self.active,
            avx2: self.avx2,
            key_set: self.key_set,
            info,
        }
    }
}

/// Whether this CPU can run the local model in useful time: AVX2 on x86_64. Other
/// architectures count as capable.
pub fn avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        true
    }
}

/// Resolve the three sources. Pure: the files are read by [`load`].
///
/// `shared` is the workgroup's file: `None` when there is none (or no workgroup), an `Err`
/// when it cannot be parsed.
fn resolve(
    shared: Option<std::result::Result<ModelSettings, String>>,
    device: &DeviceFile,
    key_set: bool,
    avx2: bool,
) -> Resolved {
    let (models, source, shadowed, invalid) = match shared {
        Some(Ok(models)) => (
            models,
            Some(ModelSource::Workgroup),
            device.models.is_some(),
            None,
        ),
        Some(Err(why)) => (
            ModelSettings::default(),
            Some(ModelSource::Workgroup),
            device.models.is_some(),
            Some(why),
        ),
        None => match &device.models {
            Some(models) => (models.clone(), Some(ModelSource::Device), false, None),
            None => (ModelSettings::default(), None, false, None),
        },
    };
    // A file that parses but breaks the rules is as unusable as one that does not parse.
    let invalid = invalid.or_else(|| models.validate().err());
    let usable = invalid.is_none();

    let local_enabled = usable && models.local.is_some() && device.local.enabled.unwrap_or(avx2);
    let remote_enabled = usable && models.remote.is_some() && device.remote.enabled.unwrap_or(true);
    let active = if remote_enabled {
        Some(Slot::Remote)
    } else if local_enabled {
        Some(Slot::Local)
    } else {
        None
    };

    let note = if let Some(why) = invalid {
        Some(EmbedNote::Invalid(why))
    } else if models.is_empty() {
        Some(EmbedNote::NotConfigured)
    } else if active.is_none() {
        let only_auto_local = models.remote.is_none() && device.local.enabled.is_none();
        Some(if only_auto_local && !avx2 {
            EmbedNote::NoAvx2
        } else {
            EmbedNote::DisabledOnDevice
        })
    } else if active == Some(Slot::Remote) && !key_set {
        Some(EmbedNote::KeyMissing)
    } else {
        None
    };

    Resolved {
        models,
        source,
        shadowed,
        device: device.settings(),
        local_enabled,
        remote_enabled,
        active,
        avx2,
        key_set,
        note,
    }
}

/// Read and resolve the settings; the config is what the bridge should serve.
pub fn load(dir: &BridgeDir, avx2: bool) -> Result<(Resolved, Option<EmbedConfig>)> {
    let shared = match Workgroup::open(dir)? {
        Some(wg) => read_shared(&wg.embedding_toml())?,
        None => None,
    };
    let device = read_device(&dir.embedding_toml());
    let key = read_key(&dir.embedding_key())?;
    let resolved = resolve(shared, &device, key.is_some(), avx2);
    let config = match resolved.active {
        Some(Slot::Remote) => resolved
            .models
            .remote
            .clone()
            .map(|model| EmbedConfig::Remote { model, key }),
        Some(Slot::Local) => resolved
            .models
            .local
            .clone()
            .map(|model| EmbedConfig::Local {
                model,
                cache_dir: device.cache_dir.clone(),
            }),
        None => None,
    };
    Ok((resolved, config))
}

/// Set or clear one slot: in the workgroup's file when the host has a workgroup, in this
/// host's `[models]` otherwise.
pub fn set_model(dir: &BridgeDir, params: EmbedModelSetParams) -> Result<()> {
    if let Some(model) = &params.model
        && model.slot() != params.slot
    {
        return Err(Error::Config(format!(
            "a {} model cannot go in the {} slot",
            model.slot(),
            params.slot
        )));
    }
    let apply = |models: &mut ModelSettings| match params.model.clone() {
        Some(SlotModel::Local(m)) => models.local = Some(m),
        Some(SlotModel::Remote(m)) => models.remote = Some(m),
        None => match params.slot {
            Slot::Local => models.local = None,
            Slot::Remote => models.remote = None,
        },
    };
    match Workgroup::open(dir)? {
        Some(wg) => {
            let path = wg.embedding_toml();
            let mut models = match read_shared(&path)? {
                Some(Ok(models)) => models,
                None => ModelSettings::default(),
                // Rewriting a file nobody can read would silently drop the other slot.
                Some(Err(why)) => {
                    return Err(Error::Config(format!(
                        "{}: {why}; fix or remove it first",
                        path.display()
                    )));
                }
            };
            apply(&mut models);
            models.validate().map_err(Error::Config)?;
            // An empty file stays: it says the workgroup has no model, which a device's own
            // `[models]` must not override.
            write_toml(
                &path,
                "# The embedding models this workgroup uses. Set with `embedding local|remote set`.",
                &models,
            )
        }
        None => {
            let path = dir.embedding_toml();
            let mut device = read_device_strict(&path)?;
            let mut models = device.models.take().unwrap_or_default();
            apply(&mut models);
            models.validate().map_err(Error::Config)?;
            device.models = (!models.is_empty()).then_some(models);
            write_device(&path, &device)
        }
    }
}

/// Switch one slot on this device; `None` is auto.
pub fn set_device(dir: &BridgeDir, slot: Slot, enabled: Option<bool>) -> Result<()> {
    let path = dir.embedding_toml();
    let mut device = read_device_strict(&path)?;
    device.switch_mut(slot).enabled = enabled;
    write_device(&path, &device)
}

/// Store the remote slot's key, readable by the owner only.
pub fn set_key(dir: &BridgeDir, key: &ApiKey) -> Result<()> {
    if key.is_empty() {
        return Err(Error::Config("the API key is empty".to_owned()));
    }
    write_private(&dir.embedding_key(), key.expose())
}

/// Remove the remote slot's key. Removing an absent key is not an error.
pub fn clear_key(dir: &BridgeDir) -> Result<()> {
    match std::fs::remove_file(dir.embedding_key()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ── files ───────────────────────────────────────────────────────────────────

/// The workgroup's file: `None` when absent, `Some(Err)` when it does not parse.
fn read_shared(path: &Path) -> Result<Option<std::result::Result<ModelSettings, String>>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// This host's file for reading: absent or unparsable means the defaults (logged).
fn read_device(path: &Path) -> DeviceFile {
    read_device_strict(path).unwrap_or_else(|err| {
        tracing::warn!(
            target: crate::logging::BRIDGE_TARGET,
            "{err}; using the default embedding settings for this device"
        );
        DeviceFile::default()
    })
}

/// This host's file for rewriting: an unparsable one is an error, not silently replaced.
fn read_device_strict(path: &Path) -> Result<DeviceFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeviceFile::default()),
        Err(e) => Err(e.into()),
    }
}

fn write_device(path: &Path, device: &DeviceFile) -> Result<()> {
    write_toml(
        path,
        "# This device's embedding settings (not synced). Set with `embedding … device`.",
        device,
    )
}

fn write_toml(path: &Path, header: &str, value: &impl Serialize) -> Result<()> {
    let body = toml::to_string_pretty(value)
        .map_err(|e| Error::Config(format!("could not encode {}: {e}", path.display())))?;
    crate::routes::write_atomic(path, header, &body)
}

fn read_key(path: &Path) -> Result<Option<ApiKey>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(ApiKey::new(text)).filter(|k| !k.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write `body` to `path` through a temporary file created owner-only, then rename.
fn write_private(path: &Path, body: &str) -> Result<()> {
    use std::io::Write;

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> LocalModel {
        LocalModel::default()
    }

    fn remote() -> RemoteModel {
        RemoteModel {
            endpoint: "https://api.example.com".into(),
            model: "m".into(),
            dimension: 8,
        }
    }

    fn both() -> ModelSettings {
        ModelSettings {
            local: Some(local()),
            remote: Some(remote()),
        }
    }

    fn device(local: Option<bool>, remote: Option<bool>) -> DeviceFile {
        DeviceFile {
            local: Switch { enabled: local },
            remote: Switch { enabled: remote },
            ..DeviceFile::default()
        }
    }

    #[test]
    fn nothing_configured_is_off() {
        let r = resolve(None, &DeviceFile::default(), false, true);
        assert_eq!((r.active, r.source), (None, None));
        assert_eq!(r.note, Some(EmbedNote::NotConfigured));
    }

    #[test]
    fn the_remote_slot_wins_when_both_are_on() {
        let r = resolve(Some(Ok(both())), &DeviceFile::default(), true, true);
        assert_eq!(r.active, Some(Slot::Remote));
        assert!(r.local_enabled && r.remote_enabled);
        assert_eq!(r.note, None);
    }

    #[test]
    fn auto_turns_the_local_slot_off_without_avx2() {
        let local_only = ModelSettings {
            local: Some(local()),
            remote: None,
        };
        let r = resolve(
            Some(Ok(local_only.clone())),
            &DeviceFile::default(),
            false,
            false,
        );
        assert_eq!(r.active, None);
        assert_eq!(r.note, Some(EmbedNote::NoAvx2));

        // An explicit switch overrides auto.
        let r = resolve(
            Some(Ok(local_only)),
            &device(Some(true), None),
            false,
            false,
        );
        assert_eq!(r.active, Some(Slot::Local));
    }

    #[test]
    fn the_remote_slot_stays_on_without_avx2() {
        let r = resolve(Some(Ok(both())), &DeviceFile::default(), true, false);
        assert!(!r.local_enabled && r.remote_enabled);
        assert_eq!(r.active, Some(Slot::Remote));
    }

    #[test]
    fn switching_the_remote_slot_off_falls_back_to_local() {
        let r = resolve(Some(Ok(both())), &device(None, Some(false)), true, true);
        assert_eq!(r.active, Some(Slot::Local));
        let r = resolve(
            Some(Ok(both())),
            &device(Some(false), Some(false)),
            true,
            true,
        );
        assert_eq!(r.active, None);
        assert_eq!(r.note, Some(EmbedNote::DisabledOnDevice));
    }

    #[test]
    fn a_remote_slot_without_a_key_still_serves_but_says_so() {
        let r = resolve(Some(Ok(both())), &DeviceFile::default(), false, true);
        assert_eq!(r.active, Some(Slot::Remote));
        assert_eq!(r.note, Some(EmbedNote::KeyMissing));
    }

    #[test]
    fn the_workgroup_wins_over_the_device_models() {
        let d = DeviceFile {
            models: Some(ModelSettings {
                local: Some(local()),
                remote: None,
            }),
            ..DeviceFile::default()
        };
        let shared = ModelSettings {
            local: None,
            remote: Some(remote()),
        };
        let r = resolve(Some(Ok(shared)), &d, true, true);
        assert_eq!(r.source, Some(ModelSource::Workgroup));
        assert!(r.shadowed);
        assert_eq!(r.active, Some(Slot::Remote));

        // Without a workgroup's file, the device's own apply.
        let r = resolve(None, &d, false, true);
        assert_eq!(r.source, Some(ModelSource::Device));
        assert_eq!(r.active, Some(Slot::Local));
    }

    #[test]
    fn invalid_settings_turn_embedding_off() {
        let r = resolve(Some(Err("bad".into())), &DeviceFile::default(), true, true);
        assert_eq!(r.active, None);
        assert_eq!(r.note, Some(EmbedNote::Invalid("bad".into())));

        let mut broken = both();
        broken.remote.as_mut().unwrap().dimension = 0;
        let r = resolve(Some(Ok(broken)), &DeviceFile::default(), true, true);
        assert_eq!(r.active, None);
        assert!(matches!(r.note, Some(EmbedNote::Invalid(_))));
    }

    fn bridge_dir() -> (tempfile::TempDir, BridgeDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().to_path_buf()).unwrap();
        (tmp, dir)
    }

    #[test]
    fn without_a_workgroup_models_go_in_the_device_file() {
        let (_tmp, dir) = bridge_dir();
        set_model(
            &dir,
            EmbedModelSetParams {
                slot: Slot::Remote,
                model: Some(SlotModel::Remote(remote())),
            },
        )
        .unwrap();
        set_device(&dir, Slot::Local, Some(false)).unwrap();
        set_key(&dir, &ApiKey::new("sk-x")).unwrap();

        let (r, config) = load(&dir, true).unwrap();
        assert_eq!(r.source, Some(ModelSource::Device));
        assert_eq!(r.device.local_enabled, Some(false));
        assert_eq!(
            config,
            Some(EmbedConfig::Remote {
                model: remote(),
                key: Some(ApiKey::new("sk-x")),
            })
        );

        // Clearing the only slot removes the table.
        set_model(
            &dir,
            EmbedModelSetParams {
                slot: Slot::Remote,
                model: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(dir.embedding_toml()).unwrap();
        assert!(!text.contains("[models"), "{text}");
        assert!(text.contains("[local]"), "{text}");
    }

    #[test]
    fn with_a_workgroup_models_go_in_its_root() {
        let (_tmp, dir) = bridge_dir();
        let wg = Workgroup::create(&dir, "wg", "this", "node").unwrap();
        set_model(
            &dir,
            EmbedModelSetParams {
                slot: Slot::Local,
                model: Some(SlotModel::Local(local())),
            },
        )
        .unwrap();
        assert!(wg.embedding_toml().is_file());
        assert!(!dir.embedding_toml().exists());
        let (r, config) = load(&dir, true).unwrap();
        assert_eq!(r.source, Some(ModelSource::Workgroup));
        assert!(matches!(config, Some(EmbedConfig::Local { .. })));

        // Clearing keeps the (now empty) file: the workgroup says "no model".
        set_model(
            &dir,
            EmbedModelSetParams {
                slot: Slot::Local,
                model: None,
            },
        )
        .unwrap();
        assert!(wg.embedding_toml().is_file());
        assert_eq!(
            load(&dir, true).unwrap().0.note,
            Some(EmbedNote::NotConfigured)
        );
    }

    #[test]
    fn a_model_must_match_its_slot_and_validate() {
        let (_tmp, dir) = bridge_dir();
        let err = set_model(
            &dir,
            EmbedModelSetParams {
                slot: Slot::Local,
                model: Some(SlotModel::Remote(remote())),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("remote model"), "{err}");
        let err = set_model(
            &dir,
            EmbedModelSetParams {
                slot: Slot::Remote,
                model: Some(SlotModel::Remote(RemoteModel {
                    dimension: 0,
                    ..remote()
                })),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("dimension"), "{err}");
        assert!(!dir.embedding_toml().exists(), "nothing written");
    }

    #[test]
    fn the_key_file_is_private_and_clearable() {
        let (_tmp, dir) = bridge_dir();
        set_key(&dir, &ApiKey::new("sk-secret\n")).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.embedding_key()).unwrap(),
            "sk-secret"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.embedding_key())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        clear_key(&dir).unwrap();
        clear_key(&dir).unwrap();
        assert!(!dir.embedding_key().exists());
        assert!(set_key(&dir, &ApiKey::new(" ")).is_err());
    }

    #[test]
    fn the_config_never_prints_the_key() {
        let config = EmbedConfig::Remote {
            model: remote(),
            key: Some(ApiKey::new("sk-secret")),
        };
        assert!(!format!("{config:?}").contains("sk-secret"));
    }

    #[test]
    fn an_unparsable_device_file_reads_as_defaults_but_is_not_overwritten() {
        let (_tmp, dir) = bridge_dir();
        std::fs::write(
            dir.embedding_toml(),
            "enabled = true\nprovider = \"local\"\n",
        )
        .unwrap();
        let (r, _) = load(&dir, true).unwrap();
        assert_eq!(r.device, DeviceSettings::default());
        assert!(set_device(&dir, Slot::Local, Some(true)).is_err());
    }
}
