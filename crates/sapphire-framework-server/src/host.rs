//! One [`LocalBackend`] per workspace root, opened lazily and closed when cold.
//!
//! The server is the only process that may open an app's cache, so it must be able to hold
//! several workspaces at once — a user runs `journal add` in one workspace and searches
//! another — while not holding every workspace it has ever seen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;

use sapphire_backend::LocalBackend;
use sapphire_workspace::{AppContext, Workspace, WorkspaceState};

use crate::error::{Error, Result};

/// How many workspaces one server keeps open by default.
pub const DEFAULT_MAX_OPEN: usize = 8;
/// How long a workspace may go unused before it is closed, by default.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(5 * 60);

struct Open {
    backend: Arc<LocalBackend>,
    last_used: Instant,
}

/// The set of workspaces this server currently has open.
pub struct WorkspaceHost {
    ctx: &'static AppContext,
    open: Mutex<HashMap<PathBuf, Open>>,
    /// Get-or-open of one root, one caller at a time.
    ///
    /// redb takes an exclusive lock on its file, so two concurrent
    /// [`WorkspaceState::open`]s of one workspace fail with `Database already open`.
    /// Held across the blocking open so the loser waits instead of failing.
    opening: AsyncMutex<()>,
    max_open: usize,
    idle: Duration,
}

impl std::fmt::Debug for WorkspaceHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceHost")
            .field("app", &self.ctx.app_name)
            .field("open", &self.open_count())
            .field("max_open", &self.max_open)
            .finish()
    }
}

impl WorkspaceHost {
    /// A host with the default limits.
    pub fn new(ctx: &'static AppContext) -> WorkspaceHost {
        WorkspaceHost::with_limits(ctx, DEFAULT_MAX_OPEN, DEFAULT_IDLE)
    }

    /// A host with explicit limits.
    pub fn with_limits(ctx: &'static AppContext, max_open: usize, idle: Duration) -> WorkspaceHost {
        WorkspaceHost {
            ctx,
            open: Mutex::new(HashMap::new()),
            opening: AsyncMutex::new(()),
            max_open: max_open.max(1),
            idle,
        }
    }

    /// The backend for `root`, opening the workspace if it is not already open.
    ///
    /// Two callers naming the same root get the same `Arc`, so the cache is opened once.
    /// Concurrent first callers are serialised: redb takes an exclusive lock on its file,
    /// so an open that overlaps another of the same workspace fails rather than waits.
    pub async fn backend(&self, root: &Path) -> Result<Arc<LocalBackend>> {
        let key = canonical(root)?;

        if let Some(backend) = self.touch(&key) {
            return Ok(backend);
        }

        // One open at a time, whatever the roots: `WorkspaceState::open` is rare next to
        // request handling, and a single async mutex keeps the map and the database
        // consistent without a per-root registry.
        let _opening = self.opening.lock().await;

        // The fast path above cannot tell whether a caller that raced us has already
        // finished its open; re-check under the lock before touching the database.
        if let Some(backend) = self.touch(&key) {
            return Ok(backend);
        }

        let ctx = self.ctx;
        let for_task = key.clone();
        let state = tokio::task::spawn_blocking(move || -> Result<WorkspaceState> {
            let workspace = Workspace::from_root(ctx, &for_task)?;
            Ok(WorkspaceState::open(workspace)?)
        })
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))??;

        let backend = Arc::new(LocalBackend::new(Arc::new(state)));

        let mut open = self.open.lock().expect("host mutex");
        // Unreachable for a second caller: the re-check above saw the winner's entry.
        let entry = open.entry(key).or_insert_with(|| Open {
            backend: Arc::clone(&backend),
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        let chosen = Arc::clone(&entry.backend);
        drop(open);

        self.enforce_limit();
        Ok(chosen)
    }

    /// Is `root` open right now? Mostly useful in tests.
    pub fn is_open(&self, root: &Path) -> bool {
        let Ok(key) = canonical(root) else {
            return false;
        };
        self.open.lock().expect("host mutex").contains_key(&key)
    }

    /// How many workspaces are open.
    pub fn open_count(&self) -> usize {
        self.open.lock().expect("host mutex").len()
    }

    /// Close every workspace unused for longer than the idle limit. Returns how many closed.
    pub fn close_idle(&self) -> usize {
        let mut open = self.open.lock().expect("host mutex");
        let before = open.len();
        open.retain(|_, entry| entry.last_used.elapsed() < self.idle);
        before - open.len()
    }

    /// Close every workspace.
    pub fn close_all(&self) {
        self.open.lock().expect("host mutex").clear();
    }

    /// The backend of `key` if it is open now; never opens one.
    pub(crate) fn open_backend(&self, key: &Path) -> Option<Arc<LocalBackend>> {
        self.touch(key)
    }

    fn touch(&self, key: &Path) -> Option<Arc<LocalBackend>> {
        let mut open = self.open.lock().expect("host mutex");
        let entry = open.get_mut(key)?;
        entry.last_used = Instant::now();
        Some(Arc::clone(&entry.backend))
    }

    fn enforce_limit(&self) {
        let mut open = self.open.lock().expect("host mutex");
        while open.len() > self.max_open {
            let Some(victim) = open
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            tracing::debug!(workspace = %victim.display(), "closing a cold workspace");
            open.remove(&victim);
        }
    }
}

/// Canonicalise a root so that two spellings of one directory are one key.
fn canonical(root: &Path) -> Result<PathBuf> {
    root.canonicalize().map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    use super::*;
    use sapphire_workspace::{AppContext, AppKind};

    use crate::test_support;

    static CTX: AppContext = AppContext::new("sapphire-hosttest");

    fn lock_env() -> MutexGuard<'static, ()> {
        // The lock is crate-wide (`test_support`): the `handlers` module's tests mutate
        // the same process environment, so one mutex must guard both. A poisoned lock
        // only means some other test panicked while holding it; the env state is
        // restored by `EnvGuard::drop`, so it is not invariant-critical here.
        test_support::lock()
    }

    /// Point the three context directories at `tmp` while holding `lock`, restoring the
    /// previous values (present or absent) when dropped — including while unwinding from
    /// a panic. Without the unconditional restore, a panicking assertion would leave the
    /// variables pointing at a temp directory the test then deletes, and the context of a
    /// later test would resolve into the void.
    struct EnvGuard {
        previous: [Option<OsString>; 3],
        _lock: MutexGuard<'static, ()>,
    }

    /// The env vars `init_ctx` writes, in the same order as `EnvGuard::previous`.
    const DIR_VARS: [&str; 3] = [
        "SAPPHIRE_HOSTTEST_CACHE_DIR",
        "SAPPHIRE_HOSTTEST_DATA_DIR",
        "SAPPHIRE_HOSTTEST_CONFIG_DIR",
    ];

    impl EnvGuard {
        fn set_dirs(lock: MutexGuard<'static, ()>, tmp: &std::path::Path) -> EnvGuard {
            let previous = DIR_VARS.map(std::env::var_os);
            let dirs = ["cache", "data", "config"].map(|cat| tmp.join(cat));
            // SAFETY (via `test_support::set`): `lock` serialises every read and write of
            // the process environment in this test binary — the `handlers` module's
            // tests share the same lock — and it is held until `drop` has restored the
            // old values.
            for (name, dir) in DIR_VARS.iter().zip(dirs) {
                test_support::set(name, &dir);
            }
            EnvGuard {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: `self._lock` still serialises the environment; it is dropped only
            // after this method returns.
            for (name, previous) in DIR_VARS.iter().zip(self.previous.iter_mut()) {
                match previous.take() {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => test_support::remove(name),
                }
            }
        }
    }

    /// Point the context's directories at a scratch location and make a workspace root.
    fn workspace(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(root.join(".sapphire-hosttest")).unwrap();
        root
    }

    fn init_ctx(lock: MutexGuard<'static, ()>, tmp: &std::path::Path) -> EnvGuard {
        let guard = EnvGuard::set_dirs(lock, tmp);
        CTX.init(AppKind::Server);
        guard
    }

    #[tokio::test]
    async fn the_same_root_yields_the_same_backend() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let root = workspace(tmp.path(), "ws");
        let host = WorkspaceHost::new(&CTX);

        let a = host.backend(&root).await.unwrap();
        let b = host.backend(&root).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "one workspace must be opened once");
        assert_eq!(host.open_count(), 1);
    }

    #[tokio::test]
    async fn two_roots_are_served_by_one_host() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let a = workspace(tmp.path(), "a");
        let b = workspace(tmp.path(), "b");
        let host = WorkspaceHost::new(&CTX);

        host.backend(&a).await.unwrap();
        host.backend(&b).await.unwrap();
        assert_eq!(host.open_count(), 2);
    }

    #[tokio::test]
    async fn a_directory_without_a_marker_is_refused() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let plain = tmp.path().join("not-a-workspace");
        std::fs::create_dir_all(&plain).unwrap();
        let host = WorkspaceHost::new(&CTX);

        assert!(host.backend(&plain).await.is_err());
        assert_eq!(
            host.open_count(),
            0,
            "a failed open must leave nothing behind"
        );
    }

    #[tokio::test]
    async fn the_least_recently_used_workspace_is_closed_when_the_limit_is_reached() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let a = workspace(tmp.path(), "a");
        let b = workspace(tmp.path(), "b");
        let c = workspace(tmp.path(), "c");
        let host = WorkspaceHost::with_limits(&CTX, 2, Duration::from_secs(3600));

        host.backend(&a).await.unwrap();
        host.backend(&b).await.unwrap();
        // Touch `a` so `b` becomes the least recently used.
        host.backend(&a).await.unwrap();
        host.backend(&c).await.unwrap();

        assert_eq!(host.open_count(), 2);
        assert!(host.is_open(&a), "the recently used workspace must stay");
        assert!(!host.is_open(&b), "the least recently used must go");
    }

    #[tokio::test]
    async fn an_idle_workspace_is_closed() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let root = workspace(tmp.path(), "ws");
        let host = WorkspaceHost::with_limits(&CTX, 8, Duration::ZERO);

        host.backend(&root).await.unwrap();
        assert_eq!(host.close_idle(), 1);
        assert_eq!(host.open_count(), 0);
    }

    #[tokio::test]
    async fn reopening_after_eviction_works() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let root = workspace(tmp.path(), "ws");
        let host = WorkspaceHost::with_limits(&CTX, 8, Duration::ZERO);

        host.backend(&root).await.unwrap();
        host.close_idle();
        // If eviction left the redb database open, this second open fails.
        host.backend(&root)
            .await
            .expect("the database must have been released");
    }

    /// Eight callers naming one workspace at once get one backend, not eight opens.
    ///
    /// redb takes an exclusive lock on its file, so a second `WorkspaceState::open` of the
    /// same workspace while the first is still open fails. The host must serialise the
    /// get-or-open of one root: this test fails with `Database already open` if two
    /// concurrent first callers both reach the open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_opens_of_one_workspace_are_serialised() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let root = workspace(tmp.path(), "ws");
        let host = Arc::new(WorkspaceHost::new(&CTX));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let host = Arc::clone(&host);
            let root = root.clone();
            tasks.push(tokio::spawn(async move { host.backend(&root).await }));
        }
        let mut backends = Vec::new();
        for task in tasks {
            backends.push(task.await.expect("the task").expect("the open"));
        }
        assert!(
            backends.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])),
            "every caller must receive the same backend"
        );
        assert_eq!(host.open_count(), 1);
    }

    #[tokio::test]
    async fn a_relative_and_an_absolute_spelling_of_one_root_are_the_same_workspace() {
        let guard = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(guard, tmp.path());
        let root = workspace(tmp.path(), "ws");
        let host = WorkspaceHost::new(&CTX);

        let a = host.backend(&root).await.unwrap();
        let b = host
            .backend(&root.join(".").join("..").join("ws"))
            .await
            .unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "the root must be canonicalised before it is a key"
        );
    }
}
