use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::workspace::path_uuid;

/// Application-wide context shared across all [`Workspace`](crate::Workspace) instances.
///
/// Holds the `app_name` (used for the marker directory) and a lazily-resolved
/// cache directory that is computed once per process from the host platform's
/// cache location.
///
/// # Usage
///
/// Declare a `static` instance in your application crate and pass a reference
/// to [`Workspace`](crate::Workspace) construction methods:
///
/// ```rust,ignore
/// use sapphire_workspace::AppContext;
///
/// pub static MY_CTX: AppContext = AppContext::new("my-app");
/// ```
///
/// On mobile platforms where the platform cache directory cannot be determined
/// automatically (e.g. Android, iOS), call [`set_cache_dir`](Self::set_cache_dir)
/// at app startup before opening any workspace:
///
/// ```rust,ignore
/// // Android: Context.getCacheDir() is already app-specific, pass it directly.
/// // iOS:     $HOME/Library/Caches is the sandbox root — pass it directly too.
/// MY_CTX.set_cache_dir(platform_cache_dir);
/// ```
pub struct AppContext {
    /// Application name without a leading dot.
    ///
    /// Controls the marker directory: `{root}/.{app_name}/`
    pub app_name: &'static str,
    /// App-specific cache directory.
    ///
    /// On desktop this is computed as `{platform_cache_home}/{app_name}`.
    /// On mobile, inject the platform-provided app cache dir via
    /// [`set_cache_dir`](Self::set_cache_dir); the `app_name` subdirectory
    /// is **not** appended because mobile OSes already sandbox cache paths
    /// per application.
    cache_dir: OnceLock<PathBuf>,
}

impl AppContext {
    /// Create a new context.  This is `const` so it can be used in `static`
    /// initialisers.
    pub const fn new(app_name: &'static str) -> Self {
        Self {
            app_name,
            cache_dir: OnceLock::new(),
        }
    }

    /// Override the app cache directory.
    ///
    /// Must be called **before** the first [`cache_dir`](Self::cache_dir) or
    /// [`cache_dir_for`](Self::cache_dir_for) call.  Subsequent calls are
    /// silently ignored (first writer wins).
    ///
    /// On mobile platforms the correct path is only obtainable via platform
    /// APIs at runtime.  Pass the platform-provided app cache directory
    /// directly — do **not** append `app_name` yourself, as mobile OSes
    /// already isolate cache paths per application.
    pub fn set_cache_dir(&self, path: PathBuf) {
        let _ = self.cache_dir.set(path);
    }

    /// Return the app-specific cache directory, computing it on first call.
    ///
    /// | Platform | Path |
    /// |----------|------|
    /// | Linux    | `$XDG_CACHE_HOME/{app_name}` or `~/.cache/{app_name}` |
    /// | macOS    | `~/Library/Caches/{app_name}` |
    /// | Windows  | `%LOCALAPPDATA%/{app_name}` |
    /// | iOS      | Result of [`set_cache_dir`](Self::set_cache_dir) (app sandbox root) |
    /// | Android  | Result of [`set_cache_dir`](Self::set_cache_dir) (`Context.getCacheDir()`) |
    pub fn cache_dir(&self) -> &Path {
        self.cache_dir
            .get_or_init(|| platform_cache_home().join(self.app_name))
    }

    /// Compute the cache directory for a workspace rooted at `root`.
    ///
    /// Returns `{cache_dir}/{uuid}/` where `uuid` is the stable UUIDv8
    /// derived from the canonicalized `root` path.
    pub fn cache_dir_for(&self, root: &Path) -> PathBuf {
        self.cache_dir().join(path_uuid(root).to_string())
    }

    /// Return the directory where embedding models should be cached.
    ///
    /// Computed as `{cache_dir}/models`.  On mobile platforms, set
    /// the correct cache directory with [`set_cache_dir`](Self::set_cache_dir)
    /// at startup so that this path points to a writable location.
    pub fn model_cache_dir(&self) -> PathBuf {
        self.cache_dir().join("models")
    }
}

fn platform_cache_home() -> PathBuf {
    // iOS: the process HOME is the app sandbox root; Library/Caches is the
    // standard writable cache location within it.
    #[cfg(target_os = "ios")]
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Library/Caches");
    }

    // Desktop (Linux, macOS, Windows) and Android fallback.
    // On Android, AppContext::set_cache_dir should be called at startup with
    // the path from Context.getCacheDir() before this function is reached.
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir)
}
