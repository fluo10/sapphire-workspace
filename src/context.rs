use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use uuid::Uuid;

use crate::error::Result;
use crate::workspace::path_uuid;

/// Application-wide context shared across all [`Workspace`](crate::Workspace) instances.
///
/// Holds the `app_name` (used for the marker directory) and the cache /
/// data directories.  This crate intentionally does **not** depend on
/// platform path crates (e.g. `dirs`); the host application is expected to
/// resolve the correct directories for its target and inject them via
/// [`set_cache_dir`](Self::set_cache_dir) and [`set_data_dir`](Self::set_data_dir)
/// at startup.  This keeps the library portable to mobile sandboxes where
/// the platform APIs differ.
///
/// # Usage
///
/// Declare a `static` instance in your application crate, then initialise
/// the directories before opening any workspace:
///
/// ```rust,ignore
/// use sapphire_workspace::AppContext;
///
/// pub static MY_CTX: AppContext = AppContext::new("my-app");
///
/// fn main() {
///     MY_CTX.set_cache_dir(host_cache_dir);   // e.g. dirs::cache_dir()/my-app
///     MY_CTX.set_data_dir(host_data_dir);     // e.g. dirs::data_dir()/my-app
///     // … run app …
/// }
/// ```
pub struct AppContext {
    /// Application name without a leading dot.
    ///
    /// Controls the marker directory: `{root}/.{app_name}/`
    pub app_name: &'static str,
    /// When `true`, file-operation methods on [`WorkspaceState`](crate::WorkspaceState)
    /// accept paths outside the workspace root (absolute paths or relative
    /// paths that traverse above the root).  External files are accessed via
    /// plain `std::fs` operations without updating the retrieve index or sync
    /// backend.
    ///
    /// Default: `false` — any path that resolves outside the workspace root
    /// returns [`Error::PathEscapesWorkspace`](crate::Error::PathEscapesWorkspace).
    allow_external_paths: bool,
    /// App-specific cache directory.  Set once at startup by the host app via
    /// [`set_cache_dir`](Self::set_cache_dir).
    cache_dir: OnceLock<PathBuf>,
    /// App-specific persistent data directory.  Set once at startup by the
    /// host app via [`set_data_dir`](Self::set_data_dir).
    data_dir: OnceLock<PathBuf>,
    /// Persistent per-device identifier, lazily loaded (or generated) on
    /// first call to [`device_id`](Self::device_id).
    device_id: OnceLock<Uuid>,
}

impl AppContext {
    /// Create a new context.  This is `const` so it can be used in `static`
    /// initialisers.
    pub const fn new(app_name: &'static str) -> Self {
        Self {
            app_name,
            allow_external_paths: false,
            cache_dir: OnceLock::new(),
            data_dir: OnceLock::new(),
            device_id: OnceLock::new(),
        }
    }

    /// Allow file operations on paths outside the workspace root.
    ///
    /// When enabled, [`WorkspaceState`](crate::WorkspaceState) file methods
    /// accept absolute or traversing-relative paths that resolve outside the
    /// workspace.  External files are handled with plain `std::fs` — no
    /// index or sync updates.
    pub const fn allow_external_paths(mut self) -> Self {
        self.allow_external_paths = true;
        self
    }

    /// Returns `true` if external (out-of-workspace) file access is permitted.
    pub fn allows_external_paths(&self) -> bool {
        self.allow_external_paths
    }

    /// Set the app cache directory.  Must be called once at startup before
    /// any workspace operation that reads [`cache_dir`](Self::cache_dir).
    /// Subsequent calls are silently ignored (first writer wins).
    pub fn set_cache_dir(&self, path: PathBuf) {
        let _ = self.cache_dir.set(path);
    }

    /// Return the app cache directory.
    ///
    /// # Panics
    /// Panics if [`set_cache_dir`](Self::set_cache_dir) has not been called.
    pub fn cache_dir(&self) -> &Path {
        self.cache_dir
            .get()
            .map(|p| p.as_path())
            .expect("AppContext::set_cache_dir must be called at startup")
    }

    /// Compute the cache directory for a workspace rooted at `root`.
    ///
    /// Returns `{cache_dir}/{uuid}/` where `uuid` is the stable UUIDv8
    /// derived from the canonicalized `root` path.
    pub fn cache_dir_for(&self, root: &Path) -> PathBuf {
        self.cache_dir().join(path_uuid(root).to_string())
    }

    /// Return the directory where embedding models should be cached
    /// (`{cache_dir}/models`).
    pub fn model_cache_dir(&self) -> PathBuf {
        self.cache_dir().join("models")
    }

    /// Set the app persistent-data directory.  Must be called once at
    /// startup before any workspace operation that reads
    /// [`data_dir`](Self::data_dir).  Subsequent calls are silently ignored
    /// (first writer wins).
    pub fn set_data_dir(&self, path: PathBuf) {
        let _ = self.data_dir.set(path);
    }

    /// Return the app persistent-data directory.
    ///
    /// # Panics
    /// Panics if [`set_data_dir`](Self::set_data_dir) has not been called.
    pub fn data_dir(&self) -> &Path {
        self.data_dir
            .get()
            .map(|p| p.as_path())
            .expect("AppContext::set_data_dir must be called at startup")
    }

    /// Return the persistent device id, generating and storing one on first
    /// call.  Stored at `<data_dir>/device_id` as plain UTF-8.
    ///
    /// Returns an [`Error::Io`](crate::Error::Io) if the data directory
    /// cannot be created or the file cannot be read / written.  The cached
    /// id is populated only on success, so a caller may retry after fixing
    /// the underlying filesystem issue.
    ///
    /// # Panics
    /// Panics if [`set_data_dir`](Self::set_data_dir) has not been called.
    pub fn device_id(&self) -> Result<Uuid> {
        if let Some(id) = self.device_id.get() {
            return Ok(*id);
        }
        let id = self.load_or_create_device_id()?;
        // Another thread may have raced us; if so, prefer the stored value
        // so every caller observes the same id for the process lifetime.
        Ok(match self.device_id.set(id) {
            Ok(()) => id,
            Err(_) => *self.device_id.get().expect("just lost the race"),
        })
    }

    fn load_or_create_device_id(&self) -> Result<Uuid> {
        let path = self.data_dir().join("device_id");
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(id) = contents.trim().parse::<Uuid>() {
                return Ok(id);
            }
            // Existing file is corrupt — fall through and regenerate.
        }
        let id = Uuid::now_v7();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, id.to_string())?;
        Ok(id)
    }
}
