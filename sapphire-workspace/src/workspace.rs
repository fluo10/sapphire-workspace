use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use sapphire_retrieve::db::SCHEMA_VERSION;

use crate::error::{Error, Result};

/// Default marker directory name used for workspace root detection.
pub const DEFAULT_WORKSPACE_MARKER: &str = ".sapphire-workspace";

/// A resolved workspace directory.
pub struct Workspace {
    /// Canonicalized absolute path of the workspace root.
    pub root: PathBuf,
    /// Marker directory name (e.g. `.sapphire-workspace`).
    marker: String,
}

impl Workspace {
    // ── marker-based discovery ────────────────────────────────────────────────

    /// Walk up from `start` until a directory containing `marker` is found.
    pub fn find_from_with_marker(start: &Path, marker: &str) -> Result<Self> {
        let start = start
            .canonicalize()
            .map_err(|e| Error::Access { path: start.to_owned(), source: e })?;
        let mut current = start.as_path();
        loop {
            if current.join(marker).is_dir() {
                return Ok(Self {
                    root: current.to_owned(),
                    marker: marker.to_owned(),
                });
            }
            match current.parent() {
                Some(p) => current = p,
                None => {
                    return Err(Error::MarkerNotFound {
                        marker: marker.to_owned(),
                        start: start.to_owned(),
                    })
                }
            }
        }
    }

    /// Walk up from the current working directory using `marker`.
    pub fn find_with_marker(marker: &str) -> Result<Self> {
        Self::find_from_with_marker(&std::env::current_dir()?, marker)
    }

    /// Walk up from `start` using the default marker (`.sapphire-workspace`).
    pub fn find_from(start: &Path) -> Result<Self> {
        Self::find_from_with_marker(start, DEFAULT_WORKSPACE_MARKER)
    }

    /// Walk up from the current working directory using the default marker.
    pub fn find() -> Result<Self> {
        Self::find_with_marker(DEFAULT_WORKSPACE_MARKER)
    }

    /// Open a workspace at `root` that already has `marker` dir present.
    ///
    /// Returns an error if the marker directory does not exist.
    pub fn from_root_with_marker(root: &Path, marker: &str) -> Result<Self> {
        let root = root
            .canonicalize()
            .map_err(|e| Error::Access { path: root.to_owned(), source: e })?;
        if !root.join(marker).is_dir() {
            return Err(Error::MarkerDirMissing {
                marker: marker.to_owned(),
                root,
            });
        }
        Ok(Self {
            root,
            marker: marker.to_owned(),
        })
    }

    /// Open a workspace at `root` using the default marker.
    pub fn from_root(root: &Path) -> Result<Self> {
        Self::from_root_with_marker(root, DEFAULT_WORKSPACE_MARKER)
    }

    /// `true` if the marker directory exists under `root`.
    pub fn has_marker(&self) -> bool {
        self.root.join(&self.marker).is_dir()
    }

    /// Path to `{root}/{marker}/config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.root.join(&self.marker).join("config.toml")
    }

    /// Path to the marker directory (`{root}/{marker}`).
    pub fn marker_dir(&self) -> PathBuf {
        self.root.join(&self.marker)
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
        Ok(Self {
            root,
            marker: DEFAULT_WORKSPACE_MARKER.to_owned(),
        })
    }

    // ── cache / DB paths ──────────────────────────────────────────────────────

    /// `$XDG_CACHE_HOME/sapphire-workspace-cli/{hash16}-{basename}/`
    pub fn cache_dir(&self) -> PathBuf {
        let hash = path_hash(&self.root);
        let basename = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".to_owned());
        xdg_cache_home()
            .join("sapphire-workspace-cli")
            .join(format!("{:016x}-{}", hash, basename))
    }

    /// `cache_dir()/retrieve_v{SCHEMA_VERSION}.db`
    pub fn retrieve_db_path(&self) -> PathBuf {
        self.cache_dir()
            .join(format!("retrieve_v{SCHEMA_VERSION}.db"))
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

/// FNV-1a hash of a path (no extra crates needed).
fn path_hash(p: &Path) -> u64 {
    const OFFSET: u64 = 14695981039346656037;
    const PRIME: u64 = 1099511628211;
    let mut h = OFFSET;
    for b in p.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
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
