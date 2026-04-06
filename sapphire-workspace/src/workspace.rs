use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Application name used by default when constructing a [`Workspace`].
///
/// The corresponding marker directory name is `".sapphire-workspace"`.
pub const DEFAULT_APP_NAME: &str = "sapphire-workspace";

/// Marker directory name used for workspace root detection (legacy; kept for
/// code that constructs the marker directory directly, e.g. `init`).
///
/// Equal to `format!(".{DEFAULT_APP_NAME}")`.
pub const DEFAULT_WORKSPACE_MARKER: &str = ".sapphire-workspace";

/// A resolved workspace directory.
pub struct Workspace {
    /// Canonicalized absolute path of the workspace root.
    pub root: PathBuf,
    /// Application name (no leading dot).
    ///
    /// Determines:
    /// - The marker directory: `{root}/.{app_name}/`
    /// - The XDG cache subdirectory: `$XDG_CACHE_HOME/{app_name}/{uuid}/`
    ///
    /// Default value: [`DEFAULT_APP_NAME`] (`"sapphire-workspace"`).
    /// Pass a different value via the `_with_app_name` construction methods
    /// so that a host application stores caches under its own XDG namespace
    /// (e.g. `"sapphire-journal"`).
    app_name: &'static str,
}

impl Workspace {
    // ── marker-based discovery ────────────────────────────────────────────────

    /// Walk up from `start` until a directory containing `.{app_name}` is found.
    pub fn find_from_with_app_name(start: &Path, app_name: &'static str) -> Result<Self> {
        let start = start
            .canonicalize()
            .map_err(|e| Error::Access { path: start.to_owned(), source: e })?;
        let marker = format!(".{app_name}");
        let mut current = start.as_path();
        loop {
            if current.join(&marker).is_dir() {
                return Ok(Self { root: current.to_owned(), app_name });
            }
            match current.parent() {
                Some(p) => current = p,
                None => {
                    return Err(Error::MarkerNotFound {
                        marker,
                        start: start.to_owned(),
                    })
                }
            }
        }
    }

    /// Walk up from the current working directory using `.{app_name}` as the marker.
    pub fn find_with_app_name(app_name: &'static str) -> Result<Self> {
        Self::find_from_with_app_name(&std::env::current_dir()?, app_name)
    }

    /// Walk up from `start` using the default app name ([`DEFAULT_APP_NAME`]).
    pub fn find_from(start: &Path) -> Result<Self> {
        Self::find_from_with_app_name(start, DEFAULT_APP_NAME)
    }

    /// Walk up from the current working directory using the default app name.
    pub fn find() -> Result<Self> {
        Self::find_with_app_name(DEFAULT_APP_NAME)
    }

    /// Open a workspace at `root` that already has `.{app_name}` dir present.
    ///
    /// Returns an error if the marker directory does not exist.
    pub fn from_root_with_app_name(root: &Path, app_name: &'static str) -> Result<Self> {
        let root = root
            .canonicalize()
            .map_err(|e| Error::Access { path: root.to_owned(), source: e })?;
        let marker = format!(".{app_name}");
        if !root.join(&marker).is_dir() {
            return Err(Error::MarkerDirMissing { marker, root });
        }
        Ok(Self { root, app_name })
    }

    /// Open a workspace at `root` using the default app name.
    pub fn from_root(root: &Path) -> Result<Self> {
        Self::from_root_with_app_name(root, DEFAULT_APP_NAME)
    }

    /// `true` if the marker directory (`.{app_name}`) exists under `root`.
    pub fn has_marker(&self) -> bool {
        self.root.join(format!(".{}", self.app_name)).is_dir()
    }

    /// Path to `{root}/.{app_name}/config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.marker_dir().join("config.toml")
    }

    /// Path to the marker directory (`{root}/.{app_name}`).
    pub fn marker_dir(&self) -> PathBuf {
        self.root.join(format!(".{}", self.app_name))
    }

    // ── legacy resolution (no marker required) ────────────────────────────────

    /// Resolve the workspace directory (no marker required):
    /// 1. `explicit` parameter (no confirmation prompt)
    /// 2. `SAPPHIRE_WORKSPACE_DIR` env var (no confirmation prompt)
    /// 3. Current working directory (TTY: ask for confirmation; non-TTY: use directly)
    pub fn resolve(explicit: Option<&Path>) -> Result<Self> {
        let root = if let Some(dir) = explicit {
            dir.canonicalize()
                .map_err(|e| Error::Access { path: dir.to_owned(), source: e })?
        } else if let Ok(val) = std::env::var("SAPPHIRE_WORKSPACE_DIR") {
            if !val.is_empty() {
                let p = PathBuf::from(&val);
                p.canonicalize()
                    .map_err(|e| Error::Access { path: p.clone(), source: e })?
            } else {
                resolve_cwd()?
            }
        } else {
            resolve_cwd()?
        };
        Ok(Self { root, app_name: DEFAULT_APP_NAME })
    }

    // ── identity / cache paths ────────────────────────────────────────────────

    /// Stable UUIDv8 identifier derived from the canonicalized workspace root.
    ///
    /// The value is computed by taking the MD5 hash of the canonical path and
    /// rewriting the version/variant nibbles to produce a valid UUIDv8.  This
    /// gives a compact, collision-resistant, human-readable identifier that
    /// does not depend on any external namespace constant.
    ///
    /// The UUID is never persisted — it is recomputed from the filesystem path
    /// on every call.  It is stable for the lifetime of the root directory.
    pub fn uuid(&self) -> uuid::Uuid {
        path_uuid(&self.root)
    }

    /// `$XDG_CACHE_HOME/{app_name}/{uuid}/`
    ///
    /// The UUID is a stable UUIDv8 derived from the canonicalized workspace
    /// root path (see [`uuid`](Self::uuid)).  The `app_name` component allows
    /// different host applications to keep their caches under separate XDG
    /// namespaces (e.g. `"sapphire-journal"`).
    pub fn cache_dir(&self) -> PathBuf {
        xdg_cache_home()
            .join(self.app_name)
            .join(self.uuid().to_string())
    }

    /// Path to the SQLite retrieve database file.
    ///
    /// The filename is versioned when the `sqlite-store` feature is enabled so
    /// that schema upgrades are detected automatically.  When only `lancedb-store`
    /// (or no storage feature) is compiled in, the file is never actually opened
    /// for SQLite, so a fixed name is used.
    pub fn retrieve_db_path(&self) -> PathBuf {
        #[cfg(feature = "sqlite-store")]
        {
            use sapphire_retrieve::db::SCHEMA_VERSION;
            return self.cache_dir().join(format!("retrieve_v{SCHEMA_VERSION}.db"));
        }
        #[cfg(not(feature = "sqlite-store"))]
        self.cache_dir().join("retrieve.db")
    }
}

fn resolve_cwd() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if std::io::stdin().is_terminal() {
        eprint!(
            "No workspace specified. Use '{}'? [Y/n]: ",
            cwd.display()
        );
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let trimmed = line.trim();
        if !trimmed.is_empty() && !matches!(trimmed, "y" | "Y") {
            eprintln!("Aborted.");
            std::process::exit(1);
        }
    }
    Ok(cwd)
}

/// Stable UUIDv8 derived from the MD5 hash of a canonicalized path.
///
/// The MD5 digest (128 bit) is rewritten with the UUIDv8 version nibble
/// (`0x8`) and the RFC 4122 variant bits (`10xx`), producing a valid UUID
/// without any external namespace constant.
///
/// This function is exported so that host applications (e.g.
/// `sapphire-journal`) can compute the same stable identifier for a root
/// directory without constructing a full [`Workspace`].
pub fn path_uuid(root: &Path) -> uuid::Uuid {
    use md5::{Digest as _, Md5};
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let mut bytes: [u8; 16] = Md5::digest(canonical.as_os_str().as_encoded_bytes()).into();
    bytes[6] = (bytes[6] & 0x0f) | 0x80; // version = 8
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant = RFC 4122 (10xx)
    uuid::Uuid::from_bytes(bytes)
}

fn xdg_cache_home() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache");
    }
    std::env::temp_dir()
}
