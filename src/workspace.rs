use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::error::{Error, Result};

/// Marker directory name for the default workspace app (`"sapphire-workspace"`).
///
/// Used when creating or locating the marker directory without a custom
/// [`AppContext`] — primarily in the bundled CLI example.
pub const DEFAULT_WORKSPACE_MARKER: &str = ".sapphire-workspace";

/// A resolved workspace directory.
pub struct Workspace {
    /// Canonicalized absolute path of the workspace root.
    pub root: PathBuf,
    /// Application context providing the app name and cache base directory.
    pub ctx: &'static AppContext,
    /// Stable identifier for this workspace.
    ///
    /// Defaults to the UUIDv8 derived from the canonicalized root path (see
    /// [`path_uuid`]).  Can be overridden at construction time via
    /// [`from_root_with_uuid`](Self::from_root_with_uuid) so that callers
    /// (e.g. mobile hosts) can supply the workspace directory name directly as
    /// the identifier.
    pub uuid: uuid::Uuid,
}

impl Workspace {
    // ── marker-based discovery ────────────────────────────────────────────────

    /// Walk up from `start` until a directory containing `.{ctx.app_name}` is found.
    pub fn find_from(ctx: &'static AppContext, start: &Path) -> Result<Self> {
        let start = start.canonicalize().map_err(|e| Error::Access {
            path: start.to_owned(),
            source: e,
        })?;
        let marker = format!(".{}", ctx.app_name);
        let mut current = start.as_path();
        loop {
            if current.join(&marker).is_dir() {
                return Ok(Self {
                    root: current.to_owned(),
                    uuid: path_uuid(current),
                    ctx,
                });
            }
            match current.parent() {
                Some(p) => current = p,
                None => {
                    return Err(Error::MarkerNotFound {
                        marker,
                        start: start.to_owned(),
                    });
                }
            }
        }
    }

    /// Walk up from the current working directory using `.{ctx.app_name}` as the marker.
    pub fn find(ctx: &'static AppContext) -> Result<Self> {
        Self::find_from(ctx, &std::env::current_dir()?)
    }

    /// Open a workspace at `root` that already has `.{ctx.app_name}` dir present.
    ///
    /// Returns an error if the marker directory does not exist.
    pub fn from_root(ctx: &'static AppContext, root: &Path) -> Result<Self> {
        let root = root.canonicalize().map_err(|e| Error::Access {
            path: root.to_owned(),
            source: e,
        })?;
        let marker = format!(".{}", ctx.app_name);
        if !root.join(&marker).is_dir() {
            return Err(Error::MarkerDirMissing { marker, root });
        }
        Ok(Self {
            uuid: path_uuid(&root),
            root,
            ctx,
        })
    }

    /// `true` if the marker directory (`.{app_name}`) exists under `root`.
    pub fn has_marker(&self) -> bool {
        self.root.join(format!(".{}", self.ctx.app_name)).is_dir()
    }

    /// Path to `{root}/.{app_name}/config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.marker_dir().join("config.toml")
    }

    /// Path to the marker directory (`{root}/.{app_name}`).
    pub fn marker_dir(&self) -> PathBuf {
        self.root.join(format!(".{}", self.ctx.app_name))
    }

    // ── legacy resolution (no marker required) ────────────────────────────────

    /// Resolve the workspace directory (no marker required):
    /// 1. `explicit` parameter (no confirmation prompt)
    /// 2. `SAPPHIRE_WORKSPACE_DIR` env var (no confirmation prompt)
    /// 3. Current working directory (TTY: ask for confirmation; non-TTY: use directly)
    pub fn resolve(ctx: &'static AppContext, explicit: Option<&Path>) -> Result<Self> {
        let root = if let Some(dir) = explicit {
            dir.canonicalize().map_err(|e| Error::Access {
                path: dir.to_owned(),
                source: e,
            })?
        } else if let Ok(val) = std::env::var("SAPPHIRE_WORKSPACE_DIR") {
            if !val.is_empty() {
                let p = PathBuf::from(&val);
                p.canonicalize().map_err(|e| Error::Access {
                    path: p.clone(),
                    source: e,
                })?
            } else {
                resolve_cwd()?
            }
        } else {
            resolve_cwd()?
        };
        Ok(Self {
            uuid: path_uuid(&root),
            root,
            ctx,
        })
    }

    // ── override constructors ─────────────────────────────────────────────────

    /// Open a workspace at `root`, using `id` as the workspace UUID instead of
    /// deriving one from the path.
    ///
    /// Useful on mobile (and similar) platforms where the workspace directory
    /// name is itself a canonical unique identifier and should be reused
    /// directly as the cache-directory key.
    ///
    /// The marker directory (`.{ctx.app_name}`) must already exist under `root`.
    pub fn from_root_with_uuid(
        ctx: &'static AppContext,
        root: &Path,
        id: uuid::Uuid,
    ) -> Result<Self> {
        let root = root.canonicalize().map_err(|e| Error::Access {
            path: root.to_owned(),
            source: e,
        })?;
        let marker = format!(".{}", ctx.app_name);
        if !root.join(&marker).is_dir() {
            return Err(Error::MarkerDirMissing { marker, root });
        }
        Ok(Self {
            uuid: id,
            root,
            ctx,
        })
    }

    /// Open a workspace at `root`, using `id` as the UUID and `app_name` as
    /// the marker-directory / cache name instead of the context default.
    ///
    /// A new [`AppContext`] is heap-allocated and leaked to satisfy the
    /// `'static` lifetime requirement.  This is appropriate for long-lived
    /// application contexts (e.g. mobile app startup).
    pub fn from_root_with_uuid_with_app_name(
        root: &Path,
        id: uuid::Uuid,
        app_name: &'static str,
    ) -> Result<Self> {
        let ctx: &'static AppContext = Box::leak(Box::new(AppContext::new(app_name)));
        Self::from_root_with_uuid(ctx, root, id)
    }

    // ── identity / cache paths ────────────────────────────────────────────────

    /// Returns the workspace UUID.
    ///
    /// By default this is a stable UUIDv8 derived from the canonicalized root
    /// path (see [`path_uuid`]).  When the workspace was constructed via
    /// [`from_root_with_uuid`](Self::from_root_with_uuid) the supplied UUID is
    /// returned instead.
    pub fn uuid(&self) -> uuid::Uuid {
        self.uuid
    }

    /// `{ctx.cache_dir}/{uuid}/`
    ///
    /// Uses [`self.uuid`](Self::uuid), so the cache path matches the workspace
    /// directory name when an explicit UUID was provided at construction time.
    pub fn cache_dir(&self) -> PathBuf {
        self.ctx.cache_dir().join(self.uuid.to_string())
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
            return self
                .cache_dir()
                .join(format!("retrieve_v{SCHEMA_VERSION}.db"));
        }
        #[cfg(not(feature = "sqlite-store"))]
        self.cache_dir().join("retrieve.db")
    }
}

fn resolve_cwd() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if std::io::stdin().is_terminal() {
        eprint!("No workspace specified. Use '{}'? [Y/n]: ", cwd.display());
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
