//! Moving a `devices.toml` into a directory of one-file-per-device records.
//!
//! The migration is idempotent, deletes nothing and never overwrites an existing record, so
//! running it at every startup is safe and running it after someone has edited the new
//! records is harmless.

use std::path::Path;

use chrono::{DateTime, Utc};
use grain_id::GrainId;
use serde::Deserialize;

use crate::devices::Devices;
use crate::error::{Error, Result};

/// What a migration did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Records written into the directory.
    pub migrated: usize,
    /// Records already present, and therefore left alone.
    pub skipped: usize,
}

/// One `[[device]]` table of the old format. `user_id` is accepted and discarded.
#[derive(Debug, Deserialize)]
struct OldDevice {
    #[serde(default)]
    id: Option<GrainId>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    retired_at: Option<DateTime<Utc>>,
    #[serde(default, rename = "user_id")]
    _user_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OldFile {
    #[serde(default)]
    device: Vec<OldDevice>,
}

/// Write every device in `file` into `dir` as its own record.
pub fn migrate_single_file(file: &Path, dir: &Path) -> Result<MigrationReport> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MigrationReport::default());
        }
        Err(e) => return Err(Error::Io(e)),
    };
    let old: OldFile =
        toml::from_str(&text).map_err(|e| Error::File(format!("{}: {e}", file.display())))?;

    let mut report = MigrationReport::default();
    let existing = Devices::open(dir)?;

    for entry in old.device {
        // An id written into content must survive; one that was never written can be new.
        let id = entry.id.unwrap_or_else(GrainId::random);
        if existing.get(id).is_some() || existing.entries().iter().any(|d| d.name == entry.name) {
            report.skipped += 1;
            continue;
        }
        let device = crate::Device {
            id,
            name: entry.name,
            node_id: entry.node_id,
            description: entry.description,
            priority: crate::DEFAULT_PRIORITY,
            created_at: entry.created_at.unwrap_or_else(Utc::now),
            retired_at: entry.retired_at,
        };
        Devices::write_record(dir, &device)?;
        report.migrated += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Devices;

    const OLD: &str = "\
[[device]]
id = \"DESKTOP\"
name = \"laptop\"
created_at = \"2026-01-01T00:00:00Z\"

[[device]]
name = \"phone\"
user_id = \"someone\"
";

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("devices.toml");
        let dir = tmp.path().join("devices");
        std::fs::write(&file, OLD).unwrap();
        (tmp, file, dir)
    }

    #[test]
    fn every_record_becomes_its_own_file() {
        let (_tmp, file, dir) = setup();
        let report = migrate_single_file(&file, &dir).unwrap();
        assert_eq!(report.migrated, 2);
        assert_eq!(report.skipped, 0);

        let devices = Devices::open(&dir).unwrap();
        let mut names: Vec<&str> = devices.entries().iter().map(|d| d.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["laptop", "phone"]);
    }

    #[test]
    fn an_explicit_id_is_preserved_and_a_missing_one_is_generated() {
        let (_tmp, file, dir) = setup();
        migrate_single_file(&file, &dir).unwrap();

        let devices = Devices::open(&dir).unwrap();
        let laptop = devices.resolve("laptop").unwrap();
        assert_eq!(
            laptop.id,
            "DESKTOP".parse::<crate::GrainId>().unwrap(),
            "an id written into content must survive"
        );
        assert!(devices.resolve("phone").is_ok());
    }

    #[test]
    fn the_old_file_is_left_alone() {
        let (_tmp, file, dir) = setup();
        migrate_single_file(&file, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), OLD);
    }

    #[test]
    fn migrating_twice_changes_nothing_the_second_time() {
        let (_tmp, file, dir) = setup();
        migrate_single_file(&file, &dir).unwrap();
        let second = migrate_single_file(&file, &dir).unwrap();
        assert_eq!(second.migrated, 0);
        assert_eq!(second.skipped, 2);
        assert_eq!(Devices::open(&dir).unwrap().entries().len(), 2);
    }

    #[test]
    fn an_existing_record_is_never_overwritten() {
        let (_tmp, file, dir) = setup();
        migrate_single_file(&file, &dir).unwrap();

        // Someone renamed the migrated device afterwards.
        let mut devices = Devices::open(&dir).unwrap();
        let laptop = devices.resolve("laptop").unwrap().clone();
        let record = dir.join(laptop.file_name());
        std::fs::write(&record, "name = \"renamed\"\n").unwrap();
        devices = Devices::open(&dir).unwrap();
        assert!(devices.resolve("renamed").is_ok());

        migrate_single_file(&file, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            "name = \"renamed\"\n"
        );
    }

    #[test]
    fn a_missing_old_file_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let report =
            migrate_single_file(&tmp.path().join("absent.toml"), &tmp.path().join("d")).unwrap();
        assert_eq!(report.migrated, 0);
    }

    #[test]
    fn a_user_id_in_the_old_file_is_dropped() {
        let (_tmp, file, dir) = setup();
        migrate_single_file(&file, &dir).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            assert!(!text.contains("user_id"), "{text}");
        }
    }
}
