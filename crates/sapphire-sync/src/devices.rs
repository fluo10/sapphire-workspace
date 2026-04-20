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
//! [`DeviceRegistry::merge_device_context`] is idempotent, the losing
//! device will re-add its entry on the next workspace open — the
//! registry self-heals without custom merge logic.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result};

/// One device's entry in the registry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    /// Stable per-device UUIDv7.  Primary key; never changes.
    pub id: Uuid,
    /// Human-readable name.  Seeded from the hostname at first
    /// registration; the user can rename it via
    /// [`DeviceRegistry::set_name`] or the equivalent CLI command.
    pub name: String,
    /// System hostname at the time the record was last written.  The
    /// app-level context always wins when this field differs from the
    /// app's current value, so peers see the latest hostname after any
    /// merge.
    pub hostname: String,
    /// Cargo package name of the binary that wrote this record
    /// (e.g. `"sapphire-workspace-cli"`, `"sapphire-workspace-gui"`).
    /// Distinct from [`AppContext::app_name`](crate::Error) which names
    /// the workspace format shared across all binaries.
    pub app_id: String,
    /// Cargo package version of the binary that wrote this record.
    pub app_version: String,
    /// `std::env::consts::OS` at the time the record was last written.
    pub platform: String,
    /// `std::env::consts::ARCH` at the time the record was last written.
    pub arch: String,
    /// When this device first registered against this workspace.
    /// Per-workspace and never rewritten.
    pub registered_at: DateTime<Utc>,
    /// When any user-editable field was last updated.  Used to resolve
    /// cross-workspace propagation of the editable fields (currently
    /// just `name`).
    pub updated_at: DateTime<Utc>,
}

/// Host-detected values supplied by the app at startup.  Used to seed
/// the per-process [`DeviceContext`] and, through it, any workspace
/// registry the app opens.
#[derive(Clone, Debug)]
pub struct DeviceDefaults {
    /// System hostname; doubles as the initial value of the
    /// `DeviceContext`'s editable `name`.
    pub hostname: String,
    /// Cargo package name of the running binary
    /// (`env!("CARGO_PKG_NAME")`).
    pub app_id: String,
    /// Cargo package version of the running binary
    /// (`env!("CARGO_PKG_VERSION")`).
    pub app_version: String,
    /// `std::env::consts::OS`.
    pub platform: String,
    /// `std::env::consts::ARCH`.
    pub arch: String,
}

/// Process-wide, mutable device state held by the app.
///
/// The host-detected fields (`hostname` / `app_id` / `app_version` /
/// `platform` / `arch`) come from [`DeviceDefaults`] and always win
/// over a workspace registry's stored values — on merge we push them
/// into the registry, not the other way round.
///
/// `name` and `updated_at` are the editable pair: the newer
/// `updated_at` wins on merge.  `updated_at` is `None` until the
/// context has either observed a workspace record or been explicitly
/// renamed by the user; this "never touched" sentinel means any
/// workspace-side timestamp wins the first merge.
#[derive(Clone, Debug)]
pub struct DeviceContext {
    pub id: Uuid,
    pub name: String,
    pub hostname: String,
    pub app_id: String,
    pub app_version: String,
    pub platform: String,
    pub arch: String,
    pub updated_at: Option<DateTime<Utc>>,
}

impl DeviceContext {
    /// Combine a freshly loaded UUID with host defaults.  `name` is
    /// seeded from `defaults.hostname`; `updated_at` starts `None`.
    pub fn from_defaults(id: Uuid, defaults: DeviceDefaults) -> Self {
        Self {
            id,
            name: defaults.hostname.clone(),
            hostname: defaults.hostname,
            app_id: defaults.app_id,
            app_version: defaults.app_version,
            platform: defaults.platform,
            arch: defaults.arch,
            updated_at: None,
        }
    }
}

/// Result of merging a [`DeviceContext`] into a [`DeviceRegistry`].
#[derive(Clone, Debug)]
pub struct MergeOutcome {
    /// True iff the registry's in-memory records changed.  The caller
    /// must [`save`](DeviceRegistry::save) and stage the file when this
    /// is true.
    pub changed: bool,
    /// The merged record as it now stands in the registry.  The caller
    /// should propagate `record.name` + `record.updated_at` back to the
    /// app context when `record.updated_at` is newer than the context's.
    pub record: DeviceRecord,
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
            Err(e) => return Err(Error::Io { path, source: e }),
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

    /// Merge the app-wide [`DeviceContext`] into this registry.
    ///
    /// - No existing entry for `ctx.id`: append a new record built from
    ///   `ctx`.  `registered_at` is now.  `updated_at` is
    ///   `ctx.updated_at.unwrap_or(now)` so a rename that already
    ///   happened in another workspace (and propagated to the context)
    ///   is preserved on this fresh registration.
    /// - Existing entry: host-detected fields (`hostname` / `app_id` /
    ///   `app_version` / `platform` / `arch`) are overwritten with the
    ///   context's values — the running binary is the source of truth
    ///   for facts about itself.  `name` + `updated_at` use whichever
    ///   side has the newer `updated_at`.  `registered_at` is never
    ///   touched.
    ///
    /// Only the [`MergeOutcome::changed`] flag's "yes" case requires
    /// saving and staging.
    pub fn merge_device_context(&mut self, ctx: &DeviceContext) -> MergeOutcome {
        let now = Utc::now();
        if let Some(existing) = self.records.iter_mut().find(|r| r.id == ctx.id) {
            let host_changed = existing.hostname != ctx.hostname
                || existing.app_id != ctx.app_id
                || existing.app_version != ctx.app_version
                || existing.platform != ctx.platform
                || existing.arch != ctx.arch;
            if host_changed {
                existing.hostname = ctx.hostname.clone();
                existing.app_id = ctx.app_id.clone();
                existing.app_version = ctx.app_version.clone();
                existing.platform = ctx.platform.clone();
                existing.arch = ctx.arch.clone();
            }
            let ctx_wins_for_name = matches!(
                ctx.updated_at,
                Some(t) if t > existing.updated_at
            );
            if ctx_wins_for_name {
                existing.name = ctx.name.clone();
                existing.updated_at = ctx.updated_at.expect("just checked Some");
            }
            MergeOutcome {
                changed: host_changed || ctx_wins_for_name,
                record: existing.clone(),
            }
        } else {
            let record = DeviceRecord {
                id: ctx.id,
                name: ctx.name.clone(),
                hostname: ctx.hostname.clone(),
                app_id: ctx.app_id.clone(),
                app_version: ctx.app_version.clone(),
                platform: ctx.platform.clone(),
                arch: ctx.arch.clone(),
                registered_at: now,
                updated_at: ctx.updated_at.unwrap_or(now),
            };
            self.records.push(record.clone());
            self.sort();
            MergeOutcome {
                changed: true,
                record,
            }
        }
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

    fn defaults(hostname: &str) -> DeviceDefaults {
        DeviceDefaults {
            hostname: hostname.to_owned(),
            app_id: "sapphire-workspace-cli".to_owned(),
            app_version: "0.9.0".to_owned(),
            platform: "linux".to_owned(),
            arch: "x86_64".to_owned(),
        }
    }

    fn ctx(hostname: &str) -> DeviceContext {
        DeviceContext::from_defaults(Uuid::now_v7(), defaults(hostname))
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
    fn merge_first_time_appends_new_record() {
        let path = tmp_path("first");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let c = ctx("my-host");
        let outcome = registry.merge_device_context(&c);
        assert!(outcome.changed);
        let r = registry.lookup(c.id).unwrap();
        assert_eq!(r.name, "my-host");
        assert_eq!(r.hostname, "my-host");
        assert_eq!(r.app_id, "sapphire-workspace-cli");
        assert_eq!(r.app_version, "0.9.0");
        // Fresh registration: context had None → record uses registered_at.
        assert_eq!(r.registered_at, r.updated_at);
    }

    #[test]
    fn merge_first_time_preserves_context_updated_at() {
        // A rename that happened in workspace A propagated into the
        // context.  Opening workspace B for the first time should carry
        // that timestamp over so subsequent merges don't oscillate.
        let path = tmp_path("first-with-ts");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let mut c = ctx("host");
        c.name = "renamed".into();
        let t = Utc::now() - chrono::Duration::hours(1);
        c.updated_at = Some(t);
        let outcome = registry.merge_device_context(&c);
        assert!(outcome.changed);
        assert_eq!(outcome.record.name, "renamed");
        assert_eq!(outcome.record.updated_at, t);
    }

    #[test]
    fn merge_is_idempotent_on_unchanged_input() {
        let path = tmp_path("idem");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let c = ctx("a");
        assert!(registry.merge_device_context(&c).changed);
        let before_updated = registry.lookup(c.id).unwrap().updated_at;
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = registry.merge_device_context(&c);
        assert!(!second.changed);
        assert_eq!(registry.lookup(c.id).unwrap().updated_at, before_updated);
    }

    #[test]
    fn merge_overwrites_host_fields_without_bumping_updated_at() {
        let path = tmp_path("host-overwrite");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let c = ctx("host-a");
        registry
            .merge_device_context(&c)
            .changed
            .then_some(())
            .unwrap();
        let before = registry.lookup(c.id).unwrap().clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut bumped = c.clone();
        bumped.app_version = "2.0.0".into();
        bumped.platform = "macos".into();
        let outcome = registry.merge_device_context(&bumped);
        assert!(outcome.changed);
        let after = registry.lookup(c.id).unwrap();
        assert_eq!(after.app_version, "2.0.0");
        assert_eq!(after.platform, "macos");
        // Host-only change must not bump updated_at.
        assert_eq!(after.updated_at, before.updated_at);
        // Name unchanged.
        assert_eq!(after.name, before.name);
    }

    #[test]
    fn merge_context_wins_name_when_updated_at_newer() {
        let path = tmp_path("ctx-wins");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let c = ctx("host");
        registry.merge_device_context(&c);
        let existing_updated = registry.lookup(c.id).unwrap().updated_at;

        let mut c2 = c.clone();
        c2.name = "ctx-name".into();
        c2.updated_at = Some(existing_updated + chrono::Duration::seconds(10));
        let outcome = registry.merge_device_context(&c2);
        assert!(outcome.changed);
        assert_eq!(registry.lookup(c.id).unwrap().name, "ctx-name");
        assert_eq!(
            registry.lookup(c.id).unwrap().updated_at,
            c2.updated_at.unwrap()
        );
    }

    #[test]
    fn merge_record_wins_name_when_newer() {
        let path = tmp_path("rec-wins");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let c = ctx("host");
        registry.merge_device_context(&c);
        registry.set_name(c.id, "record-name").unwrap();
        let record_updated = registry.lookup(c.id).unwrap().updated_at;

        let mut c2 = c.clone();
        c2.name = "stale-ctx-name".into();
        c2.updated_at = Some(record_updated - chrono::Duration::seconds(10));
        let outcome = registry.merge_device_context(&c2);
        // Host fields match, name-wise record wins → no change at all.
        assert!(!outcome.changed);
        assert_eq!(registry.lookup(c.id).unwrap().name, "record-name");
    }

    #[test]
    fn merge_keeps_uuidv7_order() {
        let path = tmp_path("order");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let a_id = Uuid::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b_id = Uuid::now_v7();
        let mut b = ctx("b");
        b.id = b_id;
        let mut a = ctx("a");
        a.id = a_id;
        // Insert in reverse order — storage should still be UUIDv7 ascending.
        registry.merge_device_context(&b);
        registry.merge_device_context(&a);
        let ids: Vec<Uuid> = registry.records().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![a_id, b_id]);
        assert_eq!(registry.device_number(a_id), Some(1));
        assert_eq!(registry.device_number(b_id), Some(2));
    }

    #[test]
    fn save_load_roundtrip_preserves_all_fields() {
        let path = tmp_path("rt");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.merge_device_context(&ctx("round"));
        registry.save().unwrap();

        let reloaded = DeviceRegistry::load(&path).unwrap();
        assert_eq!(reloaded.records(), registry.records());
    }

    #[test]
    fn set_name_bumps_updated_at_only() {
        let path = tmp_path("setname");
        let _ = fs::remove_file(&path);
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let c = ctx("seed");
        registry.merge_device_context(&c);
        let before = registry.lookup(c.id).unwrap().clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        registry.set_name(c.id, "alice-laptop").unwrap();
        let after = registry.lookup(c.id).unwrap();
        assert_eq!(after.name, "alice-laptop");
        assert_eq!(after.id, before.id);
        assert_eq!(after.registered_at, before.registered_at);
        assert_eq!(after.hostname, before.hostname);
        assert_eq!(after.app_id, before.app_id);
        assert_eq!(after.app_version, before.app_version);
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
        let c = ctx("old");
        registry.merge_device_context(&c);

        let mut incoming = registry.lookup(c.id).unwrap().clone();
        incoming.name = "old-but-older".to_owned();
        incoming.updated_at =
            registry.lookup(c.id).unwrap().updated_at - chrono::Duration::seconds(10);
        assert!(!registry.update_if_newer(incoming).unwrap());
        assert_ne!(registry.lookup(c.id).unwrap().name, "old-but-older");

        let mut fresh = registry.lookup(c.id).unwrap().clone();
        fresh.name = "winner".to_owned();
        fresh.updated_at = Utc::now() + chrono::Duration::seconds(1);
        assert!(registry.update_if_newer(fresh).unwrap());
        assert_eq!(registry.lookup(c.id).unwrap().name, "winner");
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
