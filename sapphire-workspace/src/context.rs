use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::workspace::path_uuid;

/// Application-wide context shared across all [`Workspace`](crate::Workspace) instances.
///
/// Holds the `app_name` (used for the marker directory and cache namespace) and
/// a lazily-resolved cache base directory that is computed once per process from
/// the host platform's cache location.
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
/// automatically (e.g. Android), call [`set_cache_base`](Self::set_cache_base)
/// at app startup before opening any workspace:
///
/// ```rust,ignore
/// MY_CTX.set_cache_base(android_cache_dir);
/// ```
pub struct AppContext {
    /// Application name without a leading dot.
    ///
    /// Controls:
    /// - The marker directory: `{root}/.{app_name}/`
    /// - The cache sub-directory: `{cache_base}/{app_name}/{uuid}/`
    pub app_name: &'static str,
    cache_base: OnceLock<PathBuf>,
}

impl AppContext {
    /// Create a new context.  This is `const` so it can be used in `static`
    /// initialisers.
    pub const fn new(app_name: &'static str) -> Self {
        Self { app_name, cache_base: OnceLock::new() }
    }

    /// Override the cache base directory.
    ///
    /// Must be called **before** the first [`cache_base`](Self::cache_base) or
    /// [`cache_dir_for`](Self::cache_dir_for) call.  Subsequent calls are
    /// silently ignored (first writer wins).
    ///
    /// Intended for mobile platforms (Android) where the correct cache
    /// directory is only obtainable via platform APIs at runtime.
    pub fn set_cache_base(&self, path: PathBuf) {
        let _ = self.cache_base.set(path);
    }

    /// Return the platform cache base directory, computing it on first call.
    ///
    /// | Platform | Path |
    /// |----------|------|
    /// | Linux    | `$XDG_CACHE_HOME` or `~/.cache` |
    /// | macOS    | `~/Library/Caches` |
    /// | Windows  | `%LOCALAPPDATA%` |
    /// | iOS      | `$HOME/Library/Caches` (app sandbox) |
    /// | Android  | Result of [`set_cache_base`](Self::set_cache_base), or `dirs` fallback |
    pub fn cache_base(&self) -> &Path {
        self.cache_base.get_or_init(platform_cache_home)
    }

    /// Compute the cache directory for a workspace rooted at `root`.
    ///
    /// Returns `{cache_base}/{app_name}/{uuid}/` where `uuid` is the stable
    /// UUIDv8 derived from the canonicalized `root` path.
    pub fn cache_dir_for(&self, root: &Path) -> PathBuf {
        self.cache_base().join(self.app_name).join(path_uuid(root).to_string())
    }

    /// Return the directory where embedding models should be cached.
    ///
    /// Computed as `{cache_base}/{app_name}/models`.  On mobile platforms, set
    /// the correct cache base with [`set_cache_base`](Self::set_cache_base) at
    /// startup so that this path points to a writable location.
    pub fn model_cache_dir(&self) -> PathBuf {
        self.cache_base().join(self.app_name).join("models")
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
    // On Android, AppContext::set_cache_base should be called at startup with
    // the path from Context.getCacheDir() before this function is reached.
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir)
}
