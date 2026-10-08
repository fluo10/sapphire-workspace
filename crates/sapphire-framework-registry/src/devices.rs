//! The device ledger: one directory, one record file per device.
//!
//! A device's `id` is **persisted into content** — a journal entry's frontmatter
//! `updated_by` points at it, and the record's file is named after it. So
//! removing a device from the ledger is a tombstone (`retired_at`) by default;
//! only an explicit `purge` deletes a record physically.
//!
//! One file per record is what lets two hosts mutate the ledger at the same
//! moment without colliding: each writes its own file, and every mutation here
//! rewrites exactly one record.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::write_atomic;

/// Written at the top of every record file.
const HEADER: &str = "\
# A sapphire device record. The file name is the device's id.
#
# name        required. Unique within the ledger. Accepted in place of the id
#             anywhere a command asks for a device.
# node_id     optional. The device's iroh node id: 64 lowercase hex digits.
#             Filled in when the device pairs. Unique within the ledger.
# description optional. A note for you; the system never reads it.
# priority    optional, 0-255, default 1. Higher is preferred as the designated
#             device. 0: never designated or backup.
# created_at  optional. Filled in when the record is written.
# retired_at  optional. Set by `device retire`. The record stays, because
#             synced content refers to this device's id forever.
";

/// The priority a record has when it names none: every device takes part in the election.
pub const DEFAULT_PRIORITY: u8 = 1;

fn default_priority() -> u8 {
    DEFAULT_PRIORITY
}

/// One device.
///
/// Serializable because a whole record travels over the wire: the inviter hands the joiner
/// its own record over pair/1 (§3.6), so both sides hold the identical file — the ledger is
/// replicated, and two records for one device that disagree about anything would fork it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Stable id, written into synced content as `Entry.author`. It is also the
    /// record's file name, and is not repeated inside the file.
    pub id: GrainId,
    /// Human-chosen name, unique within the ledger.
    pub name: String,
    /// The device's iroh node id, 64 lowercase hex characters, once it has one.
    ///
    /// `None` for a record written before the device announced itself: the founding device
    /// of a workgroup, or a record a user added by hand.
    pub node_id: Option<String>,
    /// A note for the user; the system never reads it.
    pub description: Option<String>,
    /// How strongly this device is preferred as a workspace's designated device. `0` opts
    /// it out of the election entirely.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// When the record was created. A hand-written record without it is stamped
    /// with the moment it was first loaded.
    pub created_at: DateTime<Utc>,
    /// When the device was retired, if it was.
    pub retired_at: Option<DateTime<Utc>>,
}

impl Device {
    /// Whether this device is retired. Not an authorization check — that is the
    /// key file's job.
    pub fn is_retired(&self) -> bool {
        self.retired_at.is_some()
    }

    /// The record's file name inside the ledger directory.
    pub fn file_name(&self) -> String {
        format!("{}.toml", self.id)
    }
}

/// The on-file representation of one record. The id is the file name, so it is
/// not a field here; every field is optional so that a record can be
/// hand-written.
#[derive(Debug, Serialize, Deserialize)]
struct RawDevice {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    priority: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retired_at: Option<DateTime<Utc>>,
}

/// A ledger directory and the snapshot of it taken when it was opened.
///
/// Every mutation rewrites exactly one record file, so a change that reached a
/// *different* record after the open (a sync from another host, a hand edit) is
/// never touched — but it is still invisible to this instance. Where the
/// directory may have changed — anything but a just-started process, a
/// long-lived one especially — open it again before mutating.
#[derive(Debug)]
pub struct Devices {
    dir: PathBuf,
    entries: Vec<Device>,
}

impl Devices {
    /// Read every record in `dir`.
    ///
    /// A missing directory is an empty ledger; it is created by the first write,
    /// not here. Files that do not end in `.toml` are ignored, so a README or a
    /// sync layer's own bookkeeping can sit alongside the records.
    pub fn open(dir: &Path) -> Result<Devices> {
        let mut entries = Vec::new();
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                for entry in rd {
                    let entry = entry?;
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    let Some(stem) = name.strip_suffix(".toml") else {
                        continue;
                    };
                    let id: GrainId = stem.parse().map_err(|_| {
                        Error::File(format!(
                            "{}: the file name is not a grain-id",
                            entry.path().display()
                        ))
                    })?;
                    // The id decodes aliases and uppercase, so a hand-written record
                    // can spell its id in a way that differs from the canonical name
                    // `file_name` would write. Admitting it would let the next
                    // mutation duplicate the record under the canonical name.
                    if stem != id.to_string() {
                        return Err(Error::File(format!(
                            "{}: the file name is not the canonical spelling of its id {id}",
                            entry.path().display()
                        )));
                    }
                    let text = std::fs::read_to_string(entry.path())?;
                    let raw: RawDevice = toml::from_str(&text)
                        .map_err(|e| Error::File(format!("{}: {e}", entry.path().display())))?;
                    entries.push(Device {
                        id,
                        name: raw.name,
                        node_id: raw.node_id,
                        description: raw.description,
                        priority: raw.priority.unwrap_or(DEFAULT_PRIORITY),
                        created_at: raw.created_at.unwrap_or_else(Utc::now),
                        retired_at: raw.retired_at,
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }
        // A stable order, so listings and tests do not depend on directory iteration.
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        if let Some(dup) = first_duplicate_name(&entries) {
            return Err(Error::File(format!(
                "{}: two devices are named {dup:?}",
                dir.display()
            )));
        }
        Ok(Devices {
            dir: dir.to_owned(),
            entries,
        })
    }

    /// The directory holding the records.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entries(&self) -> &[Device] {
        &self.entries
    }

    /// Add a device. Rejects a duplicate `name` or `node_id`.
    pub fn add(
        &mut self,
        name: &str,
        node_id: Option<String>,
        description: Option<String>,
    ) -> Result<Device> {
        if self.entries.iter().any(|d| d.name == name) {
            return Err(Error::File(format!(
                "a device named {name:?} already exists"
            )));
        }
        if let Some(node) = node_id.as_deref()
            && let Some(existing) = self.by_node_id(node)
        {
            return Err(Error::File(format!(
                "node id {node} already belongs to the device {:?}",
                existing.name
            )));
        }
        let id = GrainId::random();
        if self.entries.iter().any(|d| d.id == id) {
            // Astronomically unlikely, but writing it anyway would put two
            // records under one id, and `open` refuses a ledger it cannot
            // address. Ask the caller to `add` again rather than hunting for a
            // free id.
            return Err(Error::File(format!(
                "generated id {id} collides with an existing device; try again"
            )));
        }
        let entry = Device {
            id,
            name: name.to_owned(),
            node_id,
            description,
            priority: DEFAULT_PRIORITY,
            created_at: Utc::now(),
            retired_at: None,
        };
        self.save_one(&entry)?;
        self.entries.push(entry.clone());
        self.entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entry)
    }

    pub fn get(&self, id: GrainId) -> Option<&Device> {
        self.entries.iter().find(|d| d.id == id)
    }

    /// The device with this node id, retired or not.
    ///
    /// Retired devices are included deliberately: a retired device and an
    /// unknown one are different things to the bridge, and only the caller can
    /// decide what to do with each.
    pub fn by_node_id(&self, node_id: &str) -> Option<&Device> {
        self.entries
            .iter()
            .find(|d| d.node_id.as_deref() == Some(node_id))
    }

    /// Give an existing device its node id.
    pub fn set_node_id(&mut self, selector: &str, node_id: String) -> Result<Device> {
        let i = self.index_of(selector)?;
        if let Some(existing) = self.by_node_id(&node_id)
            && existing.id != self.entries[i].id
        {
            return Err(Error::File(format!(
                "node id {node_id} already belongs to the device {:?}",
                existing.name
            )));
        }
        if self.entries[i].node_id.as_deref() == Some(node_id.as_str()) {
            return Ok(self.entries[i].clone());
        }
        let mut updated = self.entries[i].clone();
        updated.node_id = Some(node_id);
        self.save_one(&updated)?;
        self.entries[i] = updated.clone();
        Ok(updated)
    }

    /// Resolve `selector` to the position of one entry.
    ///
    /// Device names are usually 7-8 characters and often drawn from a subset of
    /// the Crockford base32 alphabet ("pendant", "speaker", "desktop"), so a
    /// name has a good chance of parsing as a grain-id. That is why a name wins:
    /// if an entry's name matches, return it; otherwise try to read the selector
    /// as a grain-id and look up by id — which here is the file name.
    ///
    /// Names and ids are each unique in the ledger, so more than one match
    /// cannot happen. When a name happens to parse as a grain-id, the name wins
    /// — this never hits the wrong device, but there is no escape hatch to force
    /// an id.
    fn index_of(&self, selector: &str) -> Result<usize> {
        // Try the name first (a 7-8 character name very often reads as a grain-id).
        if let Some(pos) = self.entries.iter().position(|d| d.name == selector) {
            return Ok(pos);
        }
        // If no name matched, try reading the selector as a grain-id.
        if let Ok(id) = selector.parse::<GrainId>()
            && let Some(pos) = self.entries.iter().position(|d| d.id == id)
        {
            return Ok(pos);
        }
        Err(Error::File(format!("no device matches {selector:?}")))
    }

    pub fn resolve(&self, selector: &str) -> Result<&Device> {
        Ok(&self.entries[self.index_of(selector)?])
    }

    /// Retire a device. The record stays, so a `device_id` baked into content
    /// keeps resolving. An already-retired device keeps its `retired_at` and is
    /// not rewritten: this instance is the snapshot from `open`, so writing
    /// unconditionally here would trample an edit that arrived after the open,
    /// all for a record that did not change.
    pub fn retire(&mut self, selector: &str) -> Result<Device> {
        let i = self.index_of(selector)?;
        if self.entries[i].retired_at.is_some() {
            return Ok(self.entries[i].clone());
        }
        let mut updated = self.entries[i].clone();
        updated.retired_at = Some(Utc::now());
        self.save_one(&updated)?;
        self.entries[i] = updated.clone();
        Ok(updated)
    }

    /// Set a device's priority. A retired device has no say in anything, so it is refused.
    pub fn set_priority(&mut self, selector: &str, priority: u8) -> Result<Device> {
        let i = self.index_of(selector)?;
        if self.entries[i].is_retired() {
            return Err(Error::File(format!(
                "the device {:?} is retired",
                self.entries[i].name
            )));
        }
        if self.entries[i].priority == priority {
            return Ok(self.entries[i].clone());
        }
        let mut updated = self.entries[i].clone();
        updated.priority = priority;
        self.save_one(&updated)?;
        self.entries[i] = updated.clone();
        Ok(updated)
    }

    /// Really delete a device. Past `updated_by` references stop resolving.
    pub fn purge(&mut self, selector: &str) -> Result<Device> {
        let i = self.index_of(selector)?;
        let removed = self.entries[i].clone();
        self.remove_one(&removed)?;
        self.entries.remove(i);
        Ok(removed)
    }

    /// Write one record into `dir` without opening the whole ledger.
    ///
    /// `workgroup join` uses it: the inviter's ledger already holds the record under this
    /// very id, so the joiner must write the same id — and not a random one [`Devices::add`]
    /// would mint — for the replicated ledger to stay one device with one id.
    pub fn write_record(dir: &Path, device: &Device) -> Result<()> {
        Devices {
            dir: dir.to_owned(),
            entries: Vec::new(),
        }
        .save_one(device)
    }

    /// Write one record. Never touches any other file.
    fn save_one(&self, device: &Device) -> Result<()> {
        let raw = RawDevice {
            name: device.name.clone(),
            node_id: device.node_id.clone(),
            description: device.description.clone(),
            priority: (device.priority != DEFAULT_PRIORITY).then_some(device.priority),
            created_at: Some(device.created_at),
            retired_at: device.retired_at,
        };
        let body = toml::to_string_pretty(&raw)
            .map_err(|e| Error::File(format!("{}: {e}", device.file_name())))?;
        write_atomic(&self.dir.join(device.file_name()), HEADER, &body)
    }

    /// Remove one record's file.
    fn remove_one(&self, device: &Device) -> Result<()> {
        match std::fs::remove_file(self.dir.join(device.file_name())) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

fn first_duplicate_name(entries: &[Device]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .find(|d| !seen.insert(d.name.as_str()))
        .map(|d| d.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices");
        (dir, path)
    }

    /// A ledger directory holding one hand-written record named by `id`.
    pub(super) fn hand_written(
        id: &str,
        contents: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = tmp();
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(format!("{id}.toml")), contents).unwrap();
        (dir, path)
    }

    #[test]
    fn add_then_reload_round_trips() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let added = devices
            .add("pendant", None, Some("worn around the neck".into()))
            .unwrap();

        let reloaded = Devices::open(&path).unwrap();

        assert_eq!(reloaded.entries(), &[added]);
    }

    #[test]
    fn a_hand_written_record_loads_and_fills_in_created_at() {
        let (_d, path) = hand_written("abcdefg", "name = \"pendant\"\n");

        let devices = Devices::open(&path).unwrap();

        assert_eq!(devices.entries().len(), 1);
        assert_eq!(devices.entries()[0].name, "pendant");
        // created_at was not in the file; the ledger fills it in.
        assert!(devices.entries()[0].created_at.timestamp() > 0);
    }

    #[test]
    fn open_rejects_two_devices_sharing_a_name() {
        let (_d, path) = tmp();
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("abcdefg.toml"), "name = \"dup\"\n").unwrap();
        std::fs::write(path.join("h1jkmn0.toml"), "name = \"dup\"\n").unwrap();

        let err = Devices::open(&path).unwrap_err();

        assert!(err.to_string().contains("dup"), "{err}");
    }

    #[test]
    fn add_rejects_a_duplicate_name() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        devices.add("pendant", None, None).unwrap();

        let err = devices.add("pendant", None, None).unwrap_err();

        assert!(err.to_string().contains("pendant"), "{err}");
    }

    #[test]
    fn resolve_finds_by_id_and_by_name() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let added = devices.add("pendant", None, None).unwrap();

        assert_eq!(devices.resolve("pendant").unwrap(), &added);
        assert_eq!(devices.resolve(&added.id.to_string()).unwrap(), &added);
    }

    #[test]
    fn resolve_errors_on_no_match() {
        let (_d, path) = tmp();
        let devices = Devices::open(&path).unwrap();
        assert!(devices.resolve("nothing").is_err());
    }

    #[test]
    fn retire_keeps_the_record_resolvable() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let added = devices.add("gone", None, None).unwrap();

        let retired = devices.retire("gone").unwrap();

        assert!(retired.retired_at.is_some());
        // device_id is baked into a journal entry's frontmatter, so retiring must
        // leave it resolvable.
        assert!(devices.get(added.id).is_some());
        let reloaded = Devices::open(&path).unwrap();
        assert!(reloaded.entries()[0].retired_at.is_some());
    }

    #[test]
    fn retire_does_not_rewrite_when_already_retired() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let device = devices.add("gone", None, None).unwrap();
        let first = devices.retire("gone").unwrap();

        // Mimic a change that reached the file after the open (a sync or a hand
        // edit; this `devices` instance knows nothing about it).
        let mut synced = std::fs::read_to_string(path.join(device.file_name())).unwrap();
        synced.push_str("# synced by another host\n");
        std::fs::write(path.join(device.file_name()), &synced).unwrap();

        let second = devices.retire("gone").unwrap();

        assert_eq!(second.retired_at, first.retired_at, "must not overwrite");
        // The early return must not re-write, or the synced line would be gone.
        let text = std::fs::read_to_string(path.join(device.file_name())).unwrap();
        assert!(
            text.contains("# synced by another host"),
            "retiring an already-retired device rewrote the record: {text}"
        );
    }

    #[test]
    fn the_header_documents_every_field() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let added = devices.add("pendant", None, None).unwrap();

        let text = std::fs::read_to_string(path.join(added.file_name())).unwrap();

        for field in ["name", "node_id", "description", "created_at", "retired_at"] {
            assert!(
                text.contains(&format!("# {field}")),
                "the header does not document {field}: {text}"
            );
        }
        // The id is not a field any more — the header says what it is instead.
        assert!(text.contains("file name is the device's id"), "{text}");
    }

    #[test]
    fn a_name_that_parses_as_a_grain_id_still_resolves_as_a_name() {
        // A 7-character device name is often made of characters Crockford base32
        // accepts, so it can read as a grain-id: "pendant", "speaker", "desktop".
        // The name-first rule makes the name match before the id.
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let name = "pendant";
        // The premise of this test: if `name` does not actually parse as a
        // grain-id, both rules (id first / name first) take the same branch and
        // this test guarantees nothing.
        assert!(
            name.parse::<GrainId>().is_ok(),
            "the premise that {name:?} parses as a grain-id no longer holds"
        );
        let added = devices.add(name, None, None).unwrap();

        // Thanks to the name-first rule, resolve(name) must match by name.
        assert_eq!(devices.resolve(name).unwrap(), &added);
        // The id resolves too — here it is the file name.
        assert_eq!(devices.resolve(&added.id.to_string()).unwrap(), &added);
    }

    #[test]
    fn a_device_name_matching_another_device_id_resolves_by_name() {
        // When a device's name equals another device's id string, the name wins.
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let first = devices.add("device1", None, None).unwrap();
        // Give the second device the first one's id as its name.
        let second = devices.add(&first.id.to_string(), None, None).unwrap();

        // resolve(first.id) returns the second device (whose name equals that id).
        assert_eq!(devices.resolve(&first.id.to_string()).unwrap(), &second);
        // There is no way to reach `first` by id, only by another route — that is
        // this trade-off. "device1" does find it.
        assert_eq!(devices.resolve("device1").unwrap(), &first);
    }

    #[test]
    fn a_record_without_priority_reads_as_the_default() {
        let (_d, path) = hand_written("abcdefg", "name = \"pendant\"\n");

        let devices = Devices::open(&path).unwrap();

        assert_eq!(devices.entries()[0].priority, DEFAULT_PRIORITY);
    }

    #[test]
    fn set_priority_round_trips_and_leaves_the_default_unwritten() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let added = devices.add("desk", None, None).unwrap();
        let text = std::fs::read_to_string(path.join(added.file_name())).unwrap();
        assert!(
            !text.contains("priority ="),
            "the default is not written: {text}"
        );

        let updated = devices.set_priority("desk", 0).unwrap();

        assert_eq!(updated.priority, 0);
        let reloaded = Devices::open(&path).unwrap();
        assert_eq!(reloaded.entries()[0].priority, 0);

        // Set it back to the default and verify the file no longer contains priority =
        let back_to_default = devices.set_priority("desk", DEFAULT_PRIORITY).unwrap();
        assert_eq!(back_to_default.priority, DEFAULT_PRIORITY);
        let text_after = std::fs::read_to_string(path.join(back_to_default.file_name())).unwrap();
        assert!(
            !text_after.contains("priority ="),
            "the default should not be written again: {text_after}"
        );
    }

    #[test]
    fn set_priority_refuses_a_retired_device() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        devices.add("gone", None, None).unwrap();
        devices.retire("gone").unwrap();

        let err = devices.set_priority("gone", 5).unwrap_err();

        assert!(err.to_string().contains("retired"), "{err}");
    }

    #[test]
    fn a_device_deserialized_without_priority_gets_the_default() {
        // This simulates receiving a Device from a peer running an older version
        // that doesn't have the priority field. When Device is deserialized from
        // the wire protocol (pair/1), the serde(default) attribute ensures missing
        // priority deserializes to DEFAULT_PRIORITY.
        // We test the actual deserialization path: RawDevice -> Device
        let raw_toml = r#"
name = "laptop"
created_at = "2026-01-01T00:00:00Z"
"#;
        let raw: RawDevice =
            toml::from_str(raw_toml).expect("deserialize RawDevice without priority field");

        // RawDevice.priority is None (the field was not provided)
        assert!(raw.priority.is_none());

        // When Device is constructed from RawDevice, the serde default is used
        // This happens automatically during deserialization due to serde(default)
        let device = Device {
            id: GrainId::random(),
            name: raw.name,
            node_id: raw.node_id,
            description: raw.description,
            priority: raw.priority.unwrap_or(DEFAULT_PRIORITY),
            created_at: raw.created_at.unwrap_or_else(Utc::now),
            retired_at: raw.retired_at,
        };

        // Verify the default priority is used when the field is missing
        assert_eq!(
            device.priority, DEFAULT_PRIORITY,
            "missing priority field in RawDevice should result in DEFAULT_PRIORITY in Device"
        );

        // This test verifies forward compatibility: when older peers send Device records
        // without priority, the serde(default) attribute ensures deserialization succeeds
        // with the default value, maintaining compatibility over the wire protocol.

        // Additionally, test direct Device deserialization: create and serialize a Device,
        // then remove the priority field and deserialize. This proves serde(default) works.
        let device_id = GrainId::random();
        let full_device = Device {
            id: device_id,
            name: "desktop".to_string(),
            node_id: None,
            description: None,
            priority: 3,
            created_at: Utc::now(),
            retired_at: None,
        };

        // Serialize to TOML and remove the priority line
        let serialized = toml::to_string(&full_device).expect("serialize Device");
        let without_priority_field = serialized
            .lines()
            .filter(|line| !line.starts_with("priority"))
            .collect::<Vec<_>>()
            .join("\n");

        // This deserialization will only succeed if serde(default) is on the priority field
        let deserialized: Device = toml::from_str(&without_priority_field)
            .expect("deserialize Device without priority - requires serde(default)");
        assert_eq!(
            deserialized.priority, DEFAULT_PRIORITY,
            "Device deserialized without priority field must use serde default"
        );
    }
}

#[cfg(test)]
mod user_removal_tests {
    use super::Devices;
    use super::tests::{hand_written, tmp};

    #[test]
    fn a_device_record_has_no_user_field() {
        let (_d, path) = tmp();

        let mut devices = Devices::open(&path).unwrap();
        let device = devices.add("desktop", None, None).unwrap();

        let text = std::fs::read_to_string(path.join(device.file_name())).unwrap();
        // The record was hand-written without a node_name either: a hand-written
        // record with only a name is valid.
        assert!(!text.contains("user_id"), "a user_id survived:\n{text}");
    }

    #[test]
    fn a_hand_written_user_id_is_ignored_rather_than_rejected() {
        let (_d, path) = hand_written("abcdefg", "name = \"laptop\"\nuser_id = \"abcdef\"\n");

        let devices = Devices::open(&path).unwrap();
        assert_eq!(devices.entries().len(), 1);
        assert_eq!(devices.entries()[0].name, "laptop");
    }
}

#[cfg(test)]
mod node_id_tests {
    use super::Devices;
    use super::tests::tmp;

    const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const NODE_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    #[test]
    fn a_node_id_round_trips_through_the_file() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let added = devices
            .add("laptop", Some(NODE_A.to_owned()), None)
            .unwrap();
        assert_eq!(added.node_id.as_deref(), Some(NODE_A));

        let reloaded = Devices::open(&path).unwrap();
        assert_eq!(reloaded.entries()[0].node_id.as_deref(), Some(NODE_A));
    }

    #[test]
    fn a_device_can_be_found_by_its_node_id() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        devices
            .add("laptop", Some(NODE_A.to_owned()), None)
            .unwrap();
        devices.add("phone", Some(NODE_B.to_owned()), None).unwrap();

        assert_eq!(devices.by_node_id(NODE_A).unwrap().name, "laptop");
        assert_eq!(devices.by_node_id(NODE_B).unwrap().name, "phone");
        assert!(devices.by_node_id("deadbeef").is_none());
    }

    #[test]
    fn a_record_without_a_node_id_loads() {
        let (_d, path) = super::tests::hand_written("abcdefg", "name = \"laptop\"\n");

        let devices = Devices::open(&path).unwrap();
        assert!(devices.entries()[0].node_id.is_none());
    }

    #[test]
    fn a_node_id_can_be_set_later() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        devices.add("laptop", None, None).unwrap();

        let updated = devices.set_node_id("laptop", NODE_A.to_owned()).unwrap();
        assert_eq!(updated.node_id.as_deref(), Some(NODE_A));
        assert_eq!(
            Devices::open(&path)
                .unwrap()
                .by_node_id(NODE_A)
                .unwrap()
                .name,
            "laptop"
        );
    }

    #[test]
    fn two_devices_cannot_share_a_node_id() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        devices
            .add("laptop", Some(NODE_A.to_owned()), None)
            .unwrap();

        let err = devices
            .add("phone", Some(NODE_A.to_owned()), None)
            .unwrap_err();
        assert!(err.to_string().contains("node id"), "{err}");
    }

    #[test]
    fn a_retired_device_keeps_its_node_id_and_is_still_found() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        devices
            .add("laptop", Some(NODE_A.to_owned()), None)
            .unwrap();
        devices.retire("laptop").unwrap();

        let found = devices
            .by_node_id(NODE_A)
            .expect("a retired device is still a record");
        assert!(
            found.is_retired(),
            "retirement is what authorization checks"
        );
    }
}

#[cfg(test)]
mod directory_tests {
    use super::Devices;

    const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn file_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn a_missing_directory_is_an_empty_ledger_and_is_not_created() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        let devices = Devices::open(&dir).unwrap();
        assert!(devices.entries().is_empty());
        assert!(!dir.exists(), "opening must not create the directory");
    }

    #[test]
    fn each_device_gets_its_own_file_named_by_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        let mut devices = Devices::open(&dir).unwrap();

        let laptop = devices
            .add("laptop", Some(NODE_A.to_owned()), None)
            .unwrap();
        let phone = devices.add("phone", None, None).unwrap();

        assert_eq!(file_names(&dir), {
            let mut want = vec![format!("{}.toml", laptop.id), format!("{}.toml", phone.id)];
            want.sort();
            want
        });
    }

    #[test]
    fn the_id_is_the_file_name_and_is_not_repeated_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        let mut devices = Devices::open(&dir).unwrap();
        let laptop = devices.add("laptop", None, None).unwrap();

        let text = std::fs::read_to_string(dir.join(laptop.file_name())).unwrap();
        assert!(text.contains("name = \"laptop\""), "{text}");
        assert!(
            !text.contains(&laptop.id.to_string()),
            "the id is the file name:\n{text}"
        );
    }

    #[test]
    fn records_reload_from_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        let mut devices = Devices::open(&dir).unwrap();
        devices
            .add("laptop", Some(NODE_A.to_owned()), None)
            .unwrap();
        devices.add("phone", None, None).unwrap();

        let reloaded = Devices::open(&dir).unwrap();
        let mut names: Vec<&str> = reloaded.entries().iter().map(|d| d.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["laptop", "phone"]);
    }

    #[test]
    fn retiring_one_device_rewrites_only_that_devices_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        let mut devices = Devices::open(&dir).unwrap();
        let laptop = devices.add("laptop", None, None).unwrap();
        let phone = devices.add("phone", None, None).unwrap();

        // Put something unknown in the phone's file, as a newer build would.
        let phone_file = dir.join(phone.file_name());
        let mut text = std::fs::read_to_string(&phone_file).unwrap();
        text.push_str("from_the_future = true\n");
        std::fs::write(&phone_file, &text).unwrap();

        devices.retire("laptop").unwrap();

        assert!(
            std::fs::read_to_string(&phone_file)
                .unwrap()
                .contains("from_the_future"),
            "an untouched record must not be rewritten"
        );
        let laptop_text = std::fs::read_to_string(dir.join(laptop.file_name())).unwrap();
        assert!(laptop_text.contains("retired_at"), "{laptop_text}");
    }

    #[test]
    fn purging_removes_exactly_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        let mut devices = Devices::open(&dir).unwrap();
        let laptop = devices.add("laptop", None, None).unwrap();
        let phone = devices.add("phone", None, None).unwrap();

        devices.purge("laptop").unwrap();

        assert!(!dir.join(laptop.file_name()).exists());
        assert!(dir.join(phone.file_name()).exists());
        assert_eq!(devices.entries().len(), 1);
    }

    #[test]
    fn a_file_whose_name_is_not_a_grain_id_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("not-an-id!.toml"), "name = \"x\"\n").unwrap();

        let err = Devices::open(&dir).unwrap_err();
        assert!(err.to_string().contains("not-an-id!"), "{err}");
    }

    #[test]
    fn a_file_named_with_a_non_canonical_id_is_refused() {
        // grain-id's decoder accepts uppercase and the i/l/o/u aliases, so a hand-
        // written DESKTOP.toml parses — but its canonical spelling differs, and every
        // later mutation would rewrite the record under a second file name.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("DESKTOP.toml"), "name = \"laptop\"\n").unwrap();

        let err = Devices::open(&dir).unwrap_err();
        assert!(err.to_string().contains("DESKTOP"), "{err}");
    }

    #[test]
    fn a_non_toml_file_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("devices");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "notes\n").unwrap();

        assert!(Devices::open(&dir).unwrap().entries().is_empty());
    }

    #[test]
    fn a_hand_written_record_is_loaded_with_a_generated_id_as_file_name() {
        let (_d, path) = super::tests::hand_written("abcdefg", "name = \"laptop\"\n");
        let devices = Devices::open(&path).unwrap();
        assert_eq!(devices.entries().len(), 1);
        assert_eq!(devices.entries()[0].id.to_string(), "abcdefg");
        // The id (the file name) is not repeated inside the record.
        let text = std::fs::read_to_string(path.join(devices.entries()[0].file_name())).unwrap();
        assert!(
            !text.contains("id = "),
            "the id must not be a field:\n{text}"
        );
    }
}
