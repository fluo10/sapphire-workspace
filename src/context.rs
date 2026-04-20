use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use sapphire_sync::{DeviceContext, DeviceDefaults};
use uuid::Uuid;

use crate::error::Result;
use crate::workspace::path_uuid;

/// Application-wide context shared across all [`Workspace`](crate::Workspace) instances.
///
/// Holds the `app_name` (used for the marker directory), cache / data
/// directories, and the per-process [`DeviceContext`].  The device
/// context caches the UUIDv7 device id plus host-detected facts
/// (hostname, running binary, OS, arch) and tracks the latest observed
/// `updated_at` for the user-editable device name so the host app can
/// propagate renames between workspaces opened in the same process.
///
/// This crate intentionally does **not** depend on platform path
/// crates (e.g. `dirs`) or on host-detection crates (e.g. `hostname`);
/// the host application is expected to resolve the correct directories
/// and gather host facts for its target, then inject them via
/// [`set_cache_dir`](Self::set_cache_dir) /
/// [`set_data_dir`](Self::set_data_dir) /
/// [`set_device_defaults`](Self::set_device_defaults) at startup.
///
/// # Usage
///
/// Declare a `static` instance in your application crate, then
/// initialise the directories and device defaults before opening any
/// workspace:
///
/// ```rust,ignore
/// use sapphire_workspace::{AppContext, DeviceDefaults};
///
/// pub static MY_CTX: AppContext = AppContext::new("my-app");
///
/// fn main() {
///     MY_CTX.set_cache_dir(host_cache_dir);
///     MY_CTX.set_data_dir(host_data_dir);
///     MY_CTX.set_device_defaults(DeviceDefaults {
///         hostname: hostname::get().unwrap().to_string_lossy().into(),
///         app_id: env!("CARGO_PKG_NAME").to_owned(),
///         app_version: env!("CARGO_PKG_VERSION").to_owned(),
///         platform: std::env::consts::OS.to_owned(),
///         arch: std::env::consts::ARCH.to_owned(),
///     });
///     // … run app …
/// }
/// ```
pub struct AppContext {
    /// Application name without a leading dot.  Controls the marker
    /// directory: `{root}/.{app_name}/`.  Shared across all binaries
    /// (CLI, GUI, etc.) that read/write the same workspace format —
    /// per-binary identity is tracked separately in
    /// [`DeviceContext::app_id`].
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
    /// Host-detected device facts supplied by the app at startup via
    /// [`set_device_defaults`](Self::set_device_defaults).  Required
    /// before any `device*` getter is called.
    device_defaults: OnceLock<DeviceDefaults>,
    /// The per-process device context.  Outer `OnceLock`: lazy init on
    /// first access, only populated on success (so a failed UUID
    /// persistence retries on the next call).  Inner `Mutex`: allows
    /// cross-workspace name propagation to mutate the cached value.
    device: OnceLock<Mutex<DeviceContext>>,
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
            device_defaults: OnceLock::new(),
            device: OnceLock::new(),
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

    /// Supply host-detected device facts.  Must be called once at
    /// startup before any `device*` getter.  Subsequent calls are
    /// silently ignored (first writer wins).
    pub fn set_device_defaults(&self, defaults: DeviceDefaults) {
        let _ = self.device_defaults.set(defaults);
    }

    /// Snapshot of the per-process device context.
    ///
    /// Lazily initialises on first call by loading (or generating) the
    /// persistent device id and combining it with the defaults supplied
    /// via [`set_device_defaults`](Self::set_device_defaults).  Returns
    /// `None` iff the UUID could not be read or written; the error is
    /// logged at `tracing::error` level.  Subsequent calls after a
    /// failure will retry, so transient filesystem issues can recover.
    ///
    /// # Panics
    /// Panics if [`set_data_dir`](Self::set_data_dir) or
    /// [`set_device_defaults`](Self::set_device_defaults) have not been
    /// called.
    pub fn device(&self) -> Option<DeviceContext> {
        if let Some(mutex) = self.device.get() {
            return Some(mutex.lock().unwrap().clone());
        }
        let defaults = self
            .device_defaults
            .get()
            .expect("AppContext::set_device_defaults must be called at startup")
            .clone();
        let id = match self.load_or_create_device_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("could not persist device_id: {e}");
                return None;
            }
        };
        let ctx = DeviceContext::from_defaults(id, defaults);
        // Lost-race-tolerant: whoever sets first wins, everyone observes the same value.
        let _ = self.device.set(Mutex::new(ctx));
        Some(self.device.get().unwrap().lock().unwrap().clone())
    }

    /// Convenience accessor for [`DeviceContext::id`].  Same failure
    /// semantics as [`device`](Self::device).
    pub fn device_id(&self) -> Option<Uuid> {
        self.device().map(|d| d.id)
    }

    /// Propagate a `(name, updated_at)` pair from a workspace's
    /// registry back to the per-process context so that any subsequent
    /// workspace the host app opens observes the rename.
    ///
    /// No-op if the context hasn't been initialised yet, or if its
    /// current `updated_at` is already greater than or equal to the
    /// supplied timestamp.  Returns `true` when the context actually
    /// changed.
    pub fn update_device_name_if_newer(&self, name: &str, updated_at: DateTime<Utc>) -> bool {
        let Some(mutex) = self.device.get() else {
            return false;
        };
        let mut guard = mutex.lock().unwrap();
        let is_newer = match guard.updated_at {
            Some(t) => t < updated_at,
            None => true,
        };
        if is_newer {
            guard.name = name.to_owned();
            guard.updated_at = Some(updated_at);
            true
        } else {
            false
        }
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
