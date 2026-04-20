//! Device registry synchronised via the marker directory.
//!
//! The registry is persisted as a JSONL file (one JSON object per line)
//! under the workspace marker directory (e.g.
//! `.sapphire-workspace/devices.jsonl`).  Each record describes one
//! device that has ever synced this workspace.  Records are sorted by
//! their UUIDv7 id, which — because UUIDv7 encodes a millisecond
//! timestamp in the high bits — means the file is effectively ordered by
//! first-registration time and the 1-based index of a record is a
//! stable "device number" across peers.
//!
//! Conflict resolution: when two devices append their initial entry at
//! the same time without syncing, the existing git merge strategy
//! (newest author timestamp wins) may drop one of the lines.  Because
//! [`DeviceRegistry::ensure_registered`] is idempotent, the losing
//! device will simply re-add its entry on the next workspace open —
//! the registry self-heals without custom merge logic.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result};

/// One device's entry in the registry.
///
/// All fields except `name` are populated at initial registration time
/// and (with the exception of `client` / `client_version` — see
/// [`DeviceRegistry::ensure_registered`]) are never rewritten.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    /// Stable per-device UUIDv7.  Primary key; never changes.
    pub id: Uuid,
    /// Human-readable name.  Initially seeded from the system hostname,
    /// but the user can override via an explicit command.
    pub name: String,
    /// System hostname at the time of registration.  Kept verbatim so
    /// renaming the machine later doesn't rewrite history.
    pub hostname: String,
    /// Cargo package name of the client that wrote this record.
    pub client: String,
    /// Cargo package version of the client that most recently touched
    /// this record (auto-updated on workspace open when the binary
    /// version changes — see [`DeviceRegistry::ensure_registered`]).
    pub client_version: String,
    /// `std::env::consts::OS` at registration time.
    pub platform: String,
    /// `std::env::consts::ARCH` at registration time.
    pub arch: String,
    /// When this device first registered against this workspace.
    pub registered_at: DateTime<Utc>,
    /// When any field was last updated.  Used to resolve cross-workspace
    /// propagation of the editable fields (currently just `name`).
    pub updated_at: DateTime<Utc>,
}

/// Machine-detected values supplied by the caller when registering a
/// device for the first time, or when reconciling `client` /
/// `client_version` on a subsequent open.
#[derive(Clone, Debug)]
pub struct DeviceDefaults {
    /// Seed value for [`DeviceRecord::name`].  Usually the hostname.
    pub name: String,
    pub hostname: String,
    pub client: String,
    pub client_version: String,
    pub platform: String,
    pub arch: String,
}

/// Registry of devices that have synced this workspace.
#[derive(Debug)]
pub struct DeviceRegistry {
    path: PathBuf,
    records: Vec<DeviceRecord>,
}

impl DeviceRegistry {
    /// Load the registry from `path`.  Missing file ⇒ empty registry.
    ///
    /// Returns [`Error::DeviceRecordParse`] on the first unparseable
    /// line.  Blank lines are skipped silently.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let records = match fs::read_to_string(&path) {
            Ok(contents) => parse_jsonl(&path, &contents)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(Error::Io { path, source: e });
            }
        };
        let mut this = Self { path, records };
        this.sort();
        Ok(this)
    }

    /// Write the registry back to disk (sorted by UUIDv7, LF-terminated).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_owned(),
                source: e,
            })?;
        }
        let mut buf = String::new();
        for record in &self.records {
            let line = serde_json::to_string(record).expect("DeviceRecord serialisation");
            buf.push_str(&line);
            buf.push('\n');
        }
        fs::write(&self.path, buf).map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })
    }

    /// On-disk path of this registry file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Registered devices, UUIDv7 (== registration-time) order.
    pub fn records(&self) -> &[DeviceRecord] {
        &self.records
    }

    /// Idempotent self-registration + lightweight reconciliation.
    ///
    /// - No existing entry for `id`: append a fresh record populated
    ///   from `defaults` with `registered_at == updated_at == now`.
    ///   Returns `Ok(true)`.
    /// - Existing entry whose `client` or `client_version` differs from
    ///   `defaults`: overwrite just those two fields (and `updated_at`).
    ///   This is the only "automatic" in-place update — every other
    ///   field stays as originally registered.  Returns `Ok(true)`.
    /// - Otherwise: leave the record alone, return `Ok(false)`.
    ///
    /// The caller is expected to [`save`](Self::save) and stage the
    /// file only when this returns `Ok(true)`.
    pub fn ensure_registered(&mut self, id: Uuid, defaults: DeviceDefaults) -> Result<bool> {
        let now = Utc::now();
        if let Some(existing) = self.records.iter_mut().find(|r| r.id == id) {
            if existing.client == defaults.client
                && existing.client_version == defaults.client_version
            {
                return Ok(false);
            }
            existing.client = defaults.client;
            existing.client_version = defaults.client_version;
            existing.updated_at = now;
            return Ok(true);
        }
        self.records.push(DeviceRecord {
            id,
            name: defaults.name,
            hostname: defaults.hostname,
            client: defaults.client,
            client_version: defaults.client_version,
            platform: defaults.platform,
            arch: defaults.arch,
            registered_at: now,
            updated_at: now,
        });
        self.sort();
        Ok(true)
    }

    /// Update the human-readable name for `id` and bump `updated_at`.
    ///
    /// Fails with [`Error::DeviceNotFound`] if the id isn't in the
    /// registry yet.  Never touches any other field.
    pub fn set_name(&mut self, id: Uuid, name: &str) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or(Error::DeviceNotFound { id })?;
        record.name = name.to_owned();
        record.updated_at = Utc::now();
        Ok(())
    }

    /// Overwrite the matching record with `record` iff the incoming
    /// `updated_at` is strictly newer.  If there is no matching record
    /// yet, the incoming record is inserted.  Intended for a GUI that
    /// holds multiple workspaces open and wants to propagate name
    /// changes between them.
    ///
    /// Returns `Ok(true)` when the stored state changed.
    pub fn update_if_newer(&mut self, record: DeviceRecord) -> Result<bool> {
        if let Some(existing) = self.records.iter_mut().find(|r| r.id == record.id) {
            if record.updated_at <= existing.updated_at {
                return Ok(false);
            }
            *existing = record;
            return Ok(true);
        }
        self.records.push(record);
        self.sort();
        Ok(true)
    }

    /// Find the record for `id`.
    pub fn lookup(&self, id: Uuid) -> Option<&DeviceRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// 1-based index of `id` in UUIDv7 order, i.e. the "Device Number"
    /// exposed to users.  `None` if `id` is not registered.
    pub fn device_number(&self, id: Uuid) -> Option<usize> {
        self.records.iter().position(|r| r.id == id).map(|i| i + 1)
    }

    fn sort(&mut self) {
        self.records.sort_by_key(|r| r.id);
    }
}

fn parse_jsonl(path: &Path, contents: &str) -> Result<Vec<DeviceRecord>> {
    let mut out = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record =
            serde_json::from_str::<DeviceRecord>(line).map_err(|e| Error::DeviceRecordParse {
                path: path.to_owned(),
                line: i + 1,
                source: e,
            })?;
        out.push(record);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(name: &str) -> DeviceDefaults {
        DeviceDefaults {
            name: name.to_owned(),
            hostname: name.to_owned(),
            client: "sapphire-workspace-cli".to_owned(),
            client_version: "0.9.0".to_owned(),
            platform: "linux".to_owned(),
            arch: "x86_64".to_owned(),
        }
    }

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sapphire-devices-test-{}-{}",
            std::process::id(),
            name
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("devices.jsonl")
    }

    #[test]
    fn load_missing_file_is_empty() {
        let path = tmp_path("missing").with_file_name("does-not-exist.jsonl");
        let registry = DeviceRegistry::load(path).unwrap();
        assert!(registry.records().is_empty());
    }

    #[test]
    fn ensure_registered_first_time_appends_with_matching_timestamps() {
        let path = tmp_path("first");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = Uuid::now_v7();
        let added = registry.ensure_registered(id, defaults("my-host")).unwrap();
        assert!(added);
        let r = registry.lookup(id).unwrap();
        assert_eq!(r.name, "my-host");
        assert_eq!(r.registered_at, r.updated_at);
    }

    #[test]
    fn ensure_registered_is_idempotent() {
        let path = tmp_path("idem");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = Uuid::now_v7();
        assert!(registry.ensure_registered(id, defaults("a")).unwrap());
        let first_updated = registry.lookup(id).unwrap().updated_at;
        // Sleep briefly so that if a bug writes Utc::now, the timestamp would differ.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(!registry.ensure_registered(id, defaults("a")).unwrap());
        assert_eq!(registry.lookup(id).unwrap().updated_at, first_updated);
    }

    #[test]
    fn ensure_registered_syncs_client_version_only() {
        let path = tmp_path("cv");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = Uuid::now_v7();
        registry.ensure_registered(id, defaults("a")).unwrap();
        let before = registry.lookup(id).unwrap().clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut new_defaults = defaults("ignored-name");
        new_defaults.client_version = "1.0.0".to_owned();
        new_defaults.hostname = "also-ignored".to_owned();
        new_defaults.platform = "macos".to_owned();
        new_defaults.arch = "aarch64".to_owned();
        assert!(registry.ensure_registered(id, new_defaults).unwrap());
        let after = registry.lookup(id).unwrap();
        assert_eq!(after.client_version, "1.0.0");
        // Unchanged fields:
        assert_eq!(after.name, before.name);
        assert_eq!(after.hostname, before.hostname);
        assert_eq!(after.platform, before.platform);
        assert_eq!(after.arch, before.arch);
        assert_eq!(after.registered_at, before.registered_at);
        assert!(after.updated_at > before.updated_at);
    }

    #[test]
    fn ensure_registered_keeps_uuidv7_order() {
        let path = tmp_path("order");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let a = Uuid::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = Uuid::now_v7();
        // Register in reversed order — storage should still be UUIDv7 ascending.
        registry.ensure_registered(b, defaults("b")).unwrap();
        registry.ensure_registered(a, defaults("a")).unwrap();
        let ids: Vec<Uuid> = registry.records().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![a, b]);
        assert_eq!(registry.device_number(a), Some(1));
        assert_eq!(registry.device_number(b), Some(2));
    }

    #[test]
    fn save_load_roundtrip_preserves_all_fields() {
        let path = tmp_path("rt");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = Uuid::now_v7();
        registry.ensure_registered(id, defaults("round")).unwrap();
        registry.save().unwrap();

        let reloaded = DeviceRegistry::load(&path).unwrap();
        assert_eq!(reloaded.records(), registry.records());
    }

    #[test]
    fn set_name_bumps_updated_at_only() {
        let path = tmp_path("setname");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = Uuid::now_v7();
        registry.ensure_registered(id, defaults("seed")).unwrap();
        let before = registry.lookup(id).unwrap().clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        registry.set_name(id, "alice-laptop").unwrap();
        let after = registry.lookup(id).unwrap();
        assert_eq!(after.name, "alice-laptop");
        assert_eq!(after.id, before.id);
        assert_eq!(after.registered_at, before.registered_at);
        assert_eq!(after.hostname, before.hostname);
        assert_eq!(after.client, before.client);
        assert_eq!(after.client_version, before.client_version);
        assert_eq!(after.platform, before.platform);
        assert_eq!(after.arch, before.arch);
        assert!(after.updated_at > before.updated_at);
    }

    #[test]
    fn set_name_unknown_id_errors() {
        let mut registry = DeviceRegistry::load(tmp_path("setname-missing")).unwrap();
        let err = registry.set_name(Uuid::now_v7(), "nope").unwrap_err();
        assert!(matches!(err, Error::DeviceNotFound { .. }));
    }

    #[test]
    fn update_if_newer_respects_updated_at() {
        let path = tmp_path("newer");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = Uuid::now_v7();
        registry.ensure_registered(id, defaults("old")).unwrap();

        let mut incoming = registry.lookup(id).unwrap().clone();
        incoming.name = "old-but-older".to_owned();
        incoming.updated_at =
            registry.lookup(id).unwrap().updated_at - chrono::Duration::seconds(10);
        assert!(!registry.update_if_newer(incoming).unwrap());
        assert_ne!(registry.lookup(id).unwrap().name, "old-but-older");

        let mut fresh = registry.lookup(id).unwrap().clone();
        fresh.name = "winner".to_owned();
        fresh.updated_at = Utc::now() + chrono::Duration::seconds(1);
        assert!(registry.update_if_newer(fresh).unwrap());
        assert_eq!(registry.lookup(id).unwrap().name, "winner");
    }

    #[test]
    fn malformed_line_reports_error_with_line_number() {
        let path = tmp_path("malformed");
        fs::write(&path, "not-json\n").unwrap();
        let err = DeviceRegistry::load(&path).unwrap_err();
        match err {
            Error::DeviceRecordParse { line, .. } => assert_eq!(line, 1),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
