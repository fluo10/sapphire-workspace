//! The host's list of this application's workspaces.
//!
//! The per-workspace registry lives in each marker and travels with the workspace; nothing
//! there says which workspaces *this host* has. A GUI has no current directory to walk up
//! from, and a restarted server has no memory of what it synced — so the server keeps this
//! file in the application's config directory, appends on `workspace.init` and `sync.map`,
//! and flips `synced` as sync is enabled and disabled.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use indexmap::IndexMap;
use sapphire_workspace::AppContext;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The file's name inside the application's config directory.
pub const HOST_REGISTRY_FILE: &str = "workspaces.toml";

/// Serialises every read-modify-write in this process. One server per app per host, so a
/// process-wide lock is the whole story.
static LOCK: Mutex<()> = Mutex::new(());

/// One workspace this host has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    /// Its canonical root.
    pub root: PathBuf,
    /// A display name, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether sync was on when last changed — what restart restore re-enables.
    #[serde(default)]
    pub synced: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    workspace: IndexMap<String, HostEntry>,
}

/// The host registry file, read and written whole.
#[derive(Clone, Debug)]
pub struct HostRegistry {
    path: PathBuf,
}

impl HostRegistry {
    /// The registry of `ctx`'s application.
    pub fn for_app(ctx: &AppContext) -> HostRegistry {
        HostRegistry::at(ctx.config_dir().join(HOST_REGISTRY_FILE))
    }

    /// The registry at an explicit path (tests, tools).
    pub fn at(path: PathBuf) -> HostRegistry {
        HostRegistry { path }
    }

    /// Every row, in insertion order.
    pub fn entries(&self) -> Result<Vec<(String, HostEntry)>> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read()?.workspace.into_iter().collect())
    }

    /// Record `root`, returning its id. An existing root keeps its id and its flags.
    ///
    /// `root` must be canonical, as [`std::fs::canonicalize`] returns it: rows are matched
    /// by exact path, so another spelling of the same directory adds a duplicate row. A new
    /// row's id is the root's directory name, slugified and made unique against the others.
    pub fn upsert(&self, root: &Path) -> Result<String> {
        self.edit(|file| upsert_in(file, root))
    }

    /// Set `root`'s `synced` flag. A root not on record is added only when `insert` is set:
    /// enabling sync records a workspace, disabling one we never had must not invent it.
    ///
    /// `root` must be canonical, as for [`upsert`](Self::upsert): another spelling of the
    /// same directory misses its row, and with `insert` adds a duplicate one.
    pub fn set_synced(&self, root: &Path, synced: bool, insert: bool) -> Result<()> {
        self.edit(|file| {
            let id = match file.workspace.iter().find(|(_, e)| e.root == root) {
                Some((id, _)) => id.clone(),
                None if insert => upsert_in(file, root),
                None => return,
            };
            if let Some(entry) = file.workspace.get_mut(&id) {
                entry.synced = synced;
            }
        })
    }

    /// Remove the row `id`, returning it.
    pub fn forget(&self, id: &str) -> Result<Option<HostEntry>> {
        self.edit(|file| file.workspace.shift_remove(id))
    }

    fn edit<T>(&self, f: impl FnOnce(&mut File) -> T) -> Result<T> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.read()?;
        let out = f(&mut file);
        self.write(&file)?;
        Ok(out)
    }

    fn read(&self) -> Result<File> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Write via a sibling temp file and a rename, so a crash never leaves half a list.
    fn write(&self, file: &File) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(Error::Io)?;
        }
        let text = toml::to_string_pretty(file)
            .map_err(|e| Error::Config(format!("{}: {e}", self.path.display())))?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(Error::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(Error::Io)
    }
}

/// Insert `root` if absent, returning its id.
fn upsert_in(file: &mut File, root: &Path) -> String {
    if let Some((id, _)) = file.workspace.iter().find(|(_, e)| e.root == root) {
        return id.clone();
    }
    let base = slug(root);
    let id = if !file.workspace.contains_key(&base) {
        base
    } else {
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|c| !file.workspace.contains_key(c))
            .expect("an unbounded range always yields a free id")
    };
    file.workspace.insert(
        id.clone(),
        HostEntry {
            root: root.to_owned(),
            name: None,
            synced: false,
        },
    );
    id
}

/// The registry id a workspace root carries: its directory name, slugified.
///
/// Two uses, with different uniqueness. The id `workspace.init` writes into the
/// workspace's own marker registry is this slug as it is — not uniquified: that registry
/// holds the one workspace, and a second `init` of one directory is the idempotent path.
/// The host registry's id starts from it too, but is made unique against the other rows
/// by [`HostRegistry::upsert`] (`notes`, `notes-2`, …).
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
    use std::path::{Path, PathBuf};

    fn reg(tmp: &tempfile::TempDir) -> HostRegistry {
        HostRegistry::at(tmp.path().join("config").join(HOST_REGISTRY_FILE))
    }

    #[test]
    fn a_missing_file_is_an_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(reg(&tmp).entries().unwrap().is_empty());
    }

    #[test]
    fn same_dir_name_gets_distinct_ids_and_reinit_keeps_them() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(&tmp);
        let a = r.upsert(Path::new("/x/a/notes")).unwrap();
        let b = r.upsert(Path::new("/x/b/notes")).unwrap();
        assert_eq!(a, "notes");
        assert_eq!(b, "notes-2");
        assert_eq!(r.upsert(Path::new("/x/b/notes")).unwrap(), "notes-2");
        assert_eq!(r.upsert(Path::new("/x/a/notes")).unwrap(), "notes");
        assert_eq!(r.entries().unwrap().len(), 2);
    }

    #[test]
    fn set_synced_flips_the_flag_and_inserts_only_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(&tmp);
        r.set_synced(Path::new("/x/ghost"), false, false).unwrap();
        assert!(
            r.entries().unwrap().is_empty(),
            "a disable must not invent a row"
        );
        r.set_synced(Path::new("/x/notes"), true, true).unwrap();
        let rows = r.entries().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.synced);
        r.set_synced(Path::new("/x/notes"), false, false).unwrap();
        assert!(!r.entries().unwrap()[0].1.synced);
    }

    #[test]
    fn forget_removes_the_row_and_returns_it() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(&tmp);
        let id = r.upsert(Path::new("/x/notes")).unwrap();
        assert_eq!(
            r.forget(&id).unwrap().unwrap().root,
            PathBuf::from("/x/notes")
        );
        assert!(r.forget(&id).unwrap().is_none());
        assert!(r.entries().unwrap().is_empty());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_an_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(&tmp);
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config").join(HOST_REGISTRY_FILE),
            "not = [toml",
        )
        .unwrap();
        assert!(r.entries().is_err());
        assert!(
            r.upsert(Path::new("/x/notes")).is_err(),
            "never overwrite what we cannot read"
        );
    }
}
