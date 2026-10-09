use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::app_dirs::{AppKind, app_dir_env_var, migrate_keys_to_data, unsplit_app_dir};
use crate::workspace::path_uuid;

/// Application-wide context shared across all [`Workspace`](crate::Workspace) instances.
///
/// Holds the `app_name` (used for the marker directory) and the cache / data /
/// config directories.
///
/// [`init`](Self::init) is the single startup entry point that fills them: it
/// resolves the three platform roots with the `dirs` crate (re-exported by the
/// framework facade for apps), applies each category's
/// `SAPPHIRE_<APP-UPPER>_<CATEGORY>_DIR` env override (an env var replaces the
/// *platform root* only — the path is simply `<platform-root>/<app_name>`),
/// and applies the one-shot migrations of [`crate::app_dirs`] — first writer
/// wins, as with [`set_cache_dir`](Self::set_cache_dir).
///
/// # Usage
///
/// Declare a `static` instance in your application crate, then initialise the
/// directories once at startup before opening any workspace:
///
/// ```rust,ignore
/// use sapphire_workspace::{AppContext, AppKind};
///
/// pub static MY_CTX: AppContext = AppContext::new("my-app");
///
/// fn main() {
///     MY_CTX.init(AppKind::Server);
///     // … run app …
/// }
/// ```
pub struct AppContext {
    /// Application name without a leading dot.  Controls the marker
    /// directory: `{root}/.{app_name}/`.  Shared across all binaries
    /// (CLI, GUI, etc.) that read/write the same workspace format.
    pub app_name: &'static str,
    /// When `true`, file-operation methods on [`WorkspaceState`](crate::WorkspaceState)
    /// accept paths outside the workspace root (absolute paths or relative
    /// paths that traverse above the root).  External files are accessed via
    /// plain `std::fs` operations without updating the retrieve index.
    ///
    /// Default: `false` — any path that resolves outside the workspace root
    /// returns [`Error::PathEscapesWorkspace`](crate::Error::PathEscapesWorkspace).
    allow_external_paths: bool,
    /// App-specific cache directory (resolved by [`init`](Self::init) or set
    /// via [`set_cache_dir`](Self::set_cache_dir)).
    cache_dir: OnceLock<PathBuf>,
    /// App-specific persistent data directory (resolved by [`init`](Self::init)
    /// or set via [`set_data_dir`](Self::set_data_dir)).
    data_dir: OnceLock<PathBuf>,
    /// App-specific config directory (resolved by [`init`](Self::init) or set
    /// via [`set_config_dir`](Self::set_config_dir)).
    config_dir: OnceLock<PathBuf>,
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
            config_dir: OnceLock::new(),
        }
    }

    /// Initialise the cache, data and config directories from the platform
    /// defaults (`dirs::cache_dir` / `dirs::data_dir` / `dirs::config_dir`,
    /// each falling back to [`std::env::temp_dir`] when unavailable), applying
    /// each category's env override and the one-shot migrations described in
    /// [`crate::app_dirs`], and store all three (first writer wins, as with
    /// [`set_cache_dir`](Self::set_cache_dir)).
    ///
    /// Each category's env var (`SAPPHIRE_<APP-UPPER>_CACHE_DIR`, `..._DATA_DIR`,
    /// `..._CONFIG_DIR` — see [`app_dir_env_var`](crate::app_dirs::app_dir_env_var))
    /// replaces the *platform root* only; the path is always
    /// `<platform-root>/<app_name>`, with no per-kind layer.
    ///
    /// Two migrations run, both best-effort — a failure is logged, never fatal
    /// at startup:
    ///
    /// - the secrets migration (`keys.toml` from the cache tree into the data
    ///   tree, once per workspace — see
    ///   [`migrate_keys_to_data`](crate::app_dirs::migrate_keys_to_data)),
    ///   which reads the pre-#129 per-kind layout, so it runs first;
    /// - the unsplit migration (`<app>/<kind>/…` moves back up to `<app>/…` —
    ///   see [`unsplit_app_dir`](crate::app_dirs::unsplit_app_dir)).
    ///
    /// `kind` now only steers the secrets migration; it never appears in a
    /// resolved path.
    pub fn init(&self, kind: AppKind) {
        let cache_app = self
            .category_root("cache", dirs::cache_dir())
            .join(self.app_name);
        let data_app = self
            .category_root("data", dirs::data_dir())
            .join(self.app_name);
        let config_app = self
            .category_root("config", dirs::config_dir())
            .join(self.app_name);

        // Secrets first, while the cache tree is still in the per-kind layout
        // the unsplit migration is about to undo: keys.toml moves from
        // `<app>/<kind>/<uuid>/` in the cache tree into the data tree. Under
        // #129 the per-kind directories already existed by the time this ran;
        // the data tree's is recreated here only when the cache tree still has
        // one to migrate from (and the unsplit pass below removes it again
        // when nothing is moved into it).
        if cache_app.join(kind.as_str()).is_dir() {
            let data_kind = data_app.join(kind.as_str());
            if let Err(err) = std::fs::create_dir_all(&data_kind) {
                tracing::warn!(
                    "could not prepare the data directory {}: {err}",
                    data_kind.display()
                );
            }
        }
        if let Err(err) = migrate_keys_to_data(&cache_app, &data_app, kind) {
            tracing::warn!("keys.toml cache-to-data migration failed: {err}");
        }

        // Undo the per-kind layout (#129): `<app>/<kind>/…` moves back up to
        // `<app>/…`, idempotently and without deleting anything.
        for (category, app_dir) in [
            ("cache", cache_app.as_path()),
            ("data", data_app.as_path()),
            ("config", config_app.as_path()),
        ] {
            if let Err(err) = unsplit_app_dir(app_dir) {
                tracing::warn!(
                    "could not prepare the {} directory {}: {err}",
                    category,
                    app_dir.display()
                );
            }
        }

        self.set_cache_dir(cache_app);
        self.set_data_dir(data_app);
        self.set_config_dir(config_app);

        // The process prints through the framework's subscriber from here on
        // (see [`crate::logging`]): first writer wins there too, so a second
        // `init` — or a context initialised after this one already installed
        // the subscriber — changes nothing.
        crate::logging::install_console(self.app_name);
    }

    /// Resolve one category's platform root: the category's env override
    /// (`SAPPHIRE_<APP-UPPER>_<CATEGORY>_DIR`, which replaces the platform
    /// root only) or the platform default, falling back to
    /// [`std::env::temp_dir`] when neither is available.
    fn category_root(&self, category: &str, platform_root: Option<PathBuf>) -> PathBuf {
        std::env::var(app_dir_env_var(self.app_name, category))
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                let p = PathBuf::from(v);
                p.clone().canonicalize().unwrap_or(p)
            })
            .or(platform_root)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Allow file operations on paths outside the workspace root.
    ///
    /// When enabled, [`WorkspaceState`](crate::WorkspaceState) file methods
    /// accept absolute or traversing-relative paths that resolve outside the
    /// workspace.  External files are handled with plain `std::fs` — no
    /// index updates.
    pub const fn allow_external_paths(mut self) -> Self {
        self.allow_external_paths = true;
        self
    }

    /// Returns `true` if external (out-of-workspace) file access is permitted.
    pub fn allows_external_paths(&self) -> bool {
        self.allow_external_paths
    }

    /// Set the app cache directory directly.  Normally [`init`](Self::init)
    /// sets it; direct injection remains for hosts that resolve storage
    /// themselves.  Subsequent calls are silently ignored (first writer wins).
    pub fn set_cache_dir(&self, path: PathBuf) {
        let _ = self.cache_dir.set(path);
    }

    /// Return the app cache directory.
    ///
    /// # Panics
    /// Panics if neither [`init`](Self::init) nor [`set_cache_dir`](Self::set_cache_dir)
    /// has been called.
    pub fn cache_dir(&self) -> &Path {
        self.cache_dir
            .get()
            .map(|p| p.as_path())
            .expect("AppContext cache dir not initialised: init() failed for this category or was never called")
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

    /// Set the app persistent-data directory directly (see
    /// [`set_cache_dir`](Self::set_cache_dir)).  Subsequent calls are
    /// silently ignored (first writer wins).
    pub fn set_data_dir(&self, path: PathBuf) {
        let _ = self.data_dir.set(path);
    }

    /// Return the app persistent-data directory.
    ///
    /// # Panics
    /// Panics if neither [`init`](Self::init) nor [`set_data_dir`](Self::set_data_dir)
    /// has been called.
    pub fn data_dir(&self) -> &Path {
        self.data_dir
            .get()
            .map(|p| p.as_path())
            .expect("AppContext data dir not initialised: init() failed for this category or was never called")
    }

    /// Return the directory the app's log file lives in (`{data_dir}/logs`).
    ///
    /// The same shape the bridge's own directory has: the log is part of the
    /// app's persistent state on this host, so it rides in the data tree.
    /// See [`crate::logging`].
    ///
    /// # Panics
    /// Panics if neither [`init`](Self::init) nor [`set_data_dir`](Self::set_data_dir)
    /// has been called.
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir().join("logs")
    }

    /// Return the app's log directory if the data directory has been resolved,
    /// and `None` while it has not (see [`log_dir`](Self::log_dir)).
    ///
    /// This is what [`crate::logging::install`] asks: a context whose directories
    /// were never resolved has nowhere to put a log, and a server started that way
    /// serves with the console layer alone rather than failing over it.
    pub fn try_log_dir(&self) -> Option<PathBuf> {
        self.data_dir.get().map(|p| p.join("logs"))
    }

    /// Set the app config directory directly (see
    /// [`set_cache_dir`](Self::set_cache_dir)).  Subsequent calls are
    /// silently ignored (first writer wins).
    pub fn set_config_dir(&self, path: PathBuf) {
        let _ = self.config_dir.set(path);
    }

    /// Return the app config directory.
    ///
    /// # Panics
    /// Panics if neither [`init`](Self::init) nor [`set_config_dir`](Self::set_config_dir)
    /// has been called.
    pub fn config_dir(&self) -> &Path {
        self.config_dir
            .get()
            .map(|p| p.as_path())
            .expect("AppContext config dir not initialised: init() failed for this category or was never called")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::AppKind;
    use crate::test_env::TestEnv;
    use tempfile::tempdir;

    #[test]
    fn init_sets_all_three_dirs_under_the_app_directory() {
        let _env = TestEnv::lock();
        let (cache, data, config) = (tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap());
        TestEnv::set("SAPPHIRE_TESTJOURNAL_CACHE_DIR", cache.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL_DATA_DIR", data.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL_CONFIG_DIR", config.path());
        let ctx: &'static AppContext = Box::leak(Box::new(AppContext::new("sapphire-testjournal")));
        ctx.init(AppKind::Server);
        // `category_root` canonicalizes the env override (#162), so the expected side must
        // match — the same pattern `workspace.rs`'s own tests already use for a canonicalizing
        // code path.
        assert_eq!(
            ctx.cache_dir(),
            cache
                .path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal")
        );
        assert_eq!(
            ctx.data_dir(),
            data.path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal")
        );
        assert_eq!(
            ctx.config_dir(),
            config
                .path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal")
        );
    }

    #[test]
    fn init_is_idempotent_first_writer_wins() {
        let _env = TestEnv::lock();
        let (cache, data, config) = (tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap());
        TestEnv::set("SAPPHIRE_TESTJOURNAL2_CACHE_DIR", cache.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL2_DATA_DIR", data.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL2_CONFIG_DIR", config.path());
        let ctx: &'static AppContext =
            Box::leak(Box::new(AppContext::new("sapphire-testjournal2")));
        ctx.init(AppKind::Server);
        ctx.init(AppKind::Cli); // first writer wins — no change
        assert_eq!(
            ctx.cache_dir(),
            cache
                .path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal2")
        );
        assert_eq!(
            ctx.data_dir(),
            data.path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal2")
        );
        assert_eq!(
            ctx.config_dir(),
            config
                .path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal2")
        );
    }

    #[test]
    fn init_keeps_a_pre_split_flat_uuid_directory_in_place() {
        let _env = TestEnv::lock();
        let (cache, data, config) = (tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap());
        let uuid = "2f1c0000-0000-8000-8000-000000000000";
        std::fs::create_dir_all(cache.path().join("sapphire-testjournal4").join(uuid)).unwrap();
        TestEnv::set("SAPPHIRE_TESTJOURNAL4_CACHE_DIR", cache.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL4_DATA_DIR", data.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL4_CONFIG_DIR", config.path());
        let ctx: &'static AppContext =
            Box::leak(Box::new(AppContext::new("sapphire-testjournal4")));
        ctx.init(AppKind::Cli);
        let app_dir = cache
            .path()
            .canonicalize()
            .unwrap()
            .join("sapphire-testjournal4");
        assert_eq!(ctx.cache_dir(), app_dir);
        assert!(
            app_dir.join(uuid).is_dir(),
            "the flat pre-#129 layout is already the target shape"
        );
        assert!(!app_dir.join("cli").exists());
    }

    #[test]
    fn init_moves_keys_toml_from_the_cache_tree_into_the_data_tree() {
        let _env = TestEnv::lock();
        let (cache, data) = (tempdir().unwrap(), tempdir().unwrap());
        let config = tempdir().unwrap();
        let uuid = "2f1c0000-0000-8000-8000-000000000000";
        let cache_uuid = cache
            .path()
            .join("sapphire-testjournal5")
            .join("server")
            .join(uuid);
        std::fs::create_dir_all(&cache_uuid).unwrap();
        std::fs::write(cache_uuid.join("keys.toml"), "secret").unwrap();
        TestEnv::set("SAPPHIRE_TESTJOURNAL5_CACHE_DIR", cache.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL5_DATA_DIR", data.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL5_CONFIG_DIR", config.path());
        let ctx: &'static AppContext =
            Box::leak(Box::new(AppContext::new("sapphire-testjournal5")));
        ctx.init(AppKind::Server);
        let data_uuid = data.path().join("sapphire-testjournal5").join(uuid);
        assert_eq!(
            std::fs::read_to_string(data_uuid.join("keys.toml")).unwrap(),
            "secret"
        );
        assert!(!cache_uuid.join("keys.toml").exists());
    }

    #[test]
    fn cache_dir_for_appends_the_workspace_uuid() {
        let _env = TestEnv::lock();
        let (cache, data, config) = (tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap());
        TestEnv::set("SAPPHIRE_TESTJOURNAL3_CACHE_DIR", cache.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL3_DATA_DIR", data.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL3_CONFIG_DIR", config.path());
        let ctx: &'static AppContext =
            Box::leak(Box::new(AppContext::new("sapphire-testjournal3")));
        ctx.init(AppKind::Cli);
        let root = tempdir().unwrap();
        let uuid = crate::path_uuid(root.path()).to_string();
        assert_eq!(
            ctx.cache_dir_for(root.path()),
            cache
                .path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal3")
                .join(uuid)
        );
    }

    #[test]
    fn model_cache_dir_is_under_the_app_cache_directory() {
        let _env = TestEnv::lock();
        let (cache, data, config) = (tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap());
        TestEnv::set("SAPPHIRE_TESTJOURNAL6_CACHE_DIR", cache.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL6_DATA_DIR", data.path());
        TestEnv::set("SAPPHIRE_TESTJOURNAL6_CONFIG_DIR", config.path());
        let ctx: &'static AppContext =
            Box::leak(Box::new(AppContext::new("sapphire-testjournal6")));
        ctx.init(AppKind::Desktop);
        assert_eq!(
            ctx.model_cache_dir(),
            cache
                .path()
                .canonicalize()
                .unwrap()
                .join("sapphire-testjournal6")
                .join("models")
        );
    }
}
