//! Which workspace this host's server serves (#215): `<config dir>/workspace.toml`.
//!
//! One server, one workspace. The choice belongs to the host, not to the workspace, so it
//! lives in the application's config directory, and a restarted server comes back to it.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use sapphire_workspace::AppContext;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The file's name inside the application's config directory.
pub const SELECTION_FILE: &str = "workspace.toml";

/// The host-wide list the server kept before #215, read once to migrate.
const LEGACY_LIST_FILE: &str = "workspaces.toml";

/// What `workspace.toml` holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// The workspace's canonical root.
    pub root: PathBuf,
    /// Whether sync is on for it.
    #[serde(default)]
    pub sync: bool,
}

/// The selection file, read and written whole.
#[derive(Clone, Debug)]
pub struct SelectionFile {
    path: PathBuf,
}

impl SelectionFile {
    /// `ctx`'s application's file.
    pub fn for_app(ctx: &AppContext) -> SelectionFile {
        SelectionFile::at(ctx.config_dir().join(SELECTION_FILE))
    }

    /// The file at an explicit path: fixtures that run several hosts in one process.
    pub fn at(path: PathBuf) -> SelectionFile {
        SelectionFile { path }
    }

    /// The selection, or `None` when there is none yet.
    ///
    /// With no file but the list an older server kept beside it, the list's first synced
    /// workspace — else its first one whose marker is still there — is the selection, and
    /// is written down. An unreadable file is an error: never overwrite what we cannot read.
    pub fn load(&self, app_name: &str) -> Result<Option<Selection>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text)
                .map(Some)
                .map_err(|e| Error::Config(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let legacy = self.path.with_file_name(LEGACY_LIST_FILE);
                let migrated = migrate(&legacy, app_name)?;
                if let Some(selection) = &migrated {
                    tracing::info!(
                        root = %selection.root.display(),
                        "one workspace per server now; {} becomes this server's workspace",
                        selection.root.display()
                    );
                    self.save(selection)?;
                }
                Ok(migrated)
            }
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Write it, via a sibling temp file and a rename.
    pub fn save(&self, selection: &Selection) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(Error::Io)?;
        }
        let text = toml::to_string_pretty(selection)
            .map_err(|e| Error::Config(format!("{}: {e}", self.path.display())))?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(Error::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(Error::Io)
    }
}

/// The selection an older server's list implies, if it has one.
fn migrate(legacy: &Path, app_name: &str) -> Result<Option<Selection>> {
    #[derive(Deserialize)]
    struct Entry {
        root: PathBuf,
        #[serde(default)]
        synced: bool,
    }
    #[derive(Default, Deserialize)]
    struct List {
        #[serde(default)]
        workspace: IndexMap<String, Entry>,
    }
    let list: List = match std::fs::read_to_string(legacy) {
        Ok(text) => toml::from_str(&text)
            .map_err(|e| Error::Config(format!("{}: {e}", legacy.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let marker = format!(".{app_name}");
    let entries: Vec<Entry> = list.workspace.into_values().collect();
    let chosen = entries
        .iter()
        .find(|e| e.synced)
        .or_else(|| entries.iter().find(|e| e.root.join(&marker).is_dir()));
    Ok(chosen.map(|e| Selection {
        root: e.root.clone(),
        sync: e.synced,
    }))
}

/// The registry id a workspace root carries in its marker's `config.toml`: its directory
/// name, slugified.
pub(crate) fn slug(root: &Path) -> String {
    let base: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_owned())
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let base = base.trim_matches('-').to_owned();
    if base.is_empty() {
        "workspace".to_owned()
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(tmp: &tempfile::TempDir) -> SelectionFile {
        SelectionFile::at(tmp.path().join("config").join(SELECTION_FILE))
    }

    #[test]
    fn no_file_is_no_selection_and_a_saved_one_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let f = file(&tmp);
        assert_eq!(f.load("app").unwrap(), None);
        let s = Selection {
            root: PathBuf::from("/x/notes"),
            sync: true,
        };
        f.save(&s).unwrap();
        assert_eq!(f.load("app").unwrap(), Some(s));
    }

    #[test]
    fn a_corrupt_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config").join(SELECTION_FILE), "root = [").unwrap();
        assert!(file(&tmp).load("app").is_err());
    }

    #[test]
    fn the_old_list_migrates_to_its_synced_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join(LEGACY_LIST_FILE),
            "[workspace.a]\nroot = \"/x/a\"\n\n[workspace.b]\nroot = \"/x/b\"\nsynced = true\n",
        )
        .unwrap();
        let f = file(&tmp);
        let s = f.load("app").unwrap().unwrap();
        assert_eq!(s.root, PathBuf::from("/x/b"));
        assert!(s.sync);
        assert!(config.join(SELECTION_FILE).is_file(), "written down");
    }

    #[test]
    fn without_a_synced_one_the_first_reachable_workspace_migrates() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let here = tmp.path().join("here");
        std::fs::create_dir_all(here.join(".app")).unwrap();
        std::fs::write(
            config.join(LEGACY_LIST_FILE),
            format!(
                "[workspace.gone]\nroot = \"/x/gone\"\n\n[workspace.here]\nroot = {:?}\n",
                here.display().to_string()
            ),
        )
        .unwrap();
        let s = file(&tmp).load("app").unwrap().unwrap();
        assert_eq!(s.root, here);
        assert!(!s.sync);
    }

    #[test]
    fn slugs_are_lowercase_dashed_names() {
        assert_eq!(slug(Path::new("/x/My Notes!")), "my-notes");
        assert_eq!(slug(Path::new("/")), "workspace");
    }
}
