//! The one workspace a server serves (#215), opened once and kept open.
//!
//! The server is the only process that may open an app's cache. It serves one workspace at
//! a time, so it holds at most one open [`LocalBackend`]; [`Current`](crate::Current)
//! decides which, and switches it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;

use sapphire_backend::{BackendEvent, LocalBackend, WorkspaceBackend};
use sapphire_workspace::{AppContext, Workspace, WorkspaceState};

use crate::error::{Error, Result};

/// Capacity of the server-wide event channel, matching a backend's own.
const EVENT_CAPACITY: usize = 128;

struct Open {
    root: PathBuf,
    /// `None` while the workspace could not be opened (its root is gone): it stays the
    /// current one, so `workspace.current` can say so, and opening is retried on use.
    backend: Option<Arc<LocalBackend>>,
}

/// The workspace this server has open.
pub struct WorkspaceHost {
    ctx: &'static AppContext,
    open: Mutex<Option<Open>>,
    /// One open at a time: redb takes an exclusive lock on its file, so two concurrent
    /// [`WorkspaceState::open`]s of one workspace fail with `Database already open`. Held
    /// across the blocking open so the loser waits instead of failing.
    opening: AsyncMutex<()>,
    /// Every event of whichever workspace is open, so a subscription outlives a switch.
    events: broadcast::Sender<(PathBuf, BackendEvent)>,
}

impl std::fmt::Debug for WorkspaceHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceHost")
            .field("app", &self.ctx.app_name)
            .field("root", &self.root())
            .finish()
    }
}

impl WorkspaceHost {
    /// A host with no workspace.
    pub fn new(ctx: &'static AppContext) -> WorkspaceHost {
        WorkspaceHost {
            ctx,
            open: Mutex::new(None),
            opening: AsyncMutex::new(()),
            events: broadcast::channel(EVENT_CAPACITY).0,
        }
    }

    /// The current workspace's root, if there is one.
    pub fn root(&self) -> Option<PathBuf> {
        self.open
            .lock()
            .expect("host mutex")
            .as_ref()
            .map(|o| o.root.clone())
    }

    /// The current workspace's backend, opening it if it is not open yet.
    pub async fn backend(&self) -> Result<Arc<LocalBackend>> {
        Ok(self.current().await?.1)
    }

    /// The current workspace's root and backend, read together, so a switch between the
    /// two reads cannot pair one workspace's root with another's backend.
    pub async fn current(&self) -> Result<(PathBuf, Arc<LocalBackend>)> {
        let root = self.root().ok_or(Error::NoWorkspace)?;
        Ok((root.clone(), self.backend_at(&root).await?))
    }

    /// The backend of `root`, when it is the current workspace; opened if need be.
    ///
    /// For the sync runtime, which works on a root it was handed: a workspace switched
    /// away from in the meantime is an error, never a second open.
    pub(crate) async fn backend_at(&self, root: &Path) -> Result<Arc<LocalBackend>> {
        if let Some(backend) = self.open_backend(root) {
            return Ok(backend);
        }
        let _opening = self.opening.lock().await;
        if let Some(backend) = self.open_backend(root) {
            return Ok(backend);
        }
        if self.root().as_deref() != Some(root) {
            return Err(Error::NoWorkspace);
        }
        let backend = self.open_state(root).await?;
        let mut open = self.open.lock().expect("host mutex");
        match open.as_mut() {
            // Still current: keep it. A switch that landed during the open wins.
            Some(o) if o.root == root => {
                o.backend = Some(Arc::clone(&backend));
                Ok(backend)
            }
            _ => Err(Error::NoWorkspace),
        }
    }

    /// The backend of `root` if it is the current workspace and open now; never opens.
    pub(crate) fn open_backend(&self, root: &Path) -> Option<Arc<LocalBackend>> {
        let open = self.open.lock().expect("host mutex");
        let o = open.as_ref()?;
        (o.root == root).then(|| o.backend.clone()).flatten()
    }

    /// Make `root` (canonical) the current workspace, opening it first.
    ///
    /// A workspace that fails to open is an error and leaves the current one as it was.
    /// With `allow_unreachable`, a root that is gone is selected anyway, unopened — the
    /// restore at start, which must keep the user's choice visible.
    pub(crate) async fn select(&self, root: &Path, allow_unreachable: bool) -> Result<()> {
        let _opening = self.opening.lock().await;
        if self.root().as_deref() == Some(root) && self.open_backend(root).is_some() {
            return Ok(());
        }
        // The cheap refusal first, while the old workspace is still open: a directory that
        // is not this app's workspace never closes anything.
        if !allow_unreachable {
            Workspace::from_root(self.ctx, root)?;
        }
        // Close the old one before opening: its index and the new one's may be the same
        // files (the same root), and an open that overlapped would fail on redb's lock.
        let previous = self.open.lock().expect("host mutex").take();
        let previous_root = previous.as_ref().map(|o| o.root.clone());
        drop(previous);
        let backend = match self.open_state(root).await {
            Ok(backend) => Some(backend),
            Err(_) if allow_unreachable => None,
            Err(err) => {
                // The old one stays current, unopened; it reopens on its next use.
                *self.open.lock().expect("host mutex") = previous_root.map(|root| Open {
                    root,
                    backend: None,
                });
                return Err(err);
            }
        };
        *self.open.lock().expect("host mutex") = Some(Open {
            root: root.to_owned(),
            backend,
        });
        Ok(())
    }

    /// Close the workspace and forget it: the host has none afterwards.
    pub fn close(&self) {
        self.open.lock().expect("host mutex").take();
    }

    /// Every event of the current workspace, and of the next after a switch.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<(PathBuf, BackendEvent)> {
        self.events.subscribe()
    }

    /// Open `root`'s state and forward its events to the host's channel.
    async fn open_state(&self, root: &Path) -> Result<Arc<LocalBackend>> {
        let ctx = self.ctx;
        let for_task = root.to_owned();
        let state = tokio::task::spawn_blocking(move || -> Result<WorkspaceState> {
            let workspace = Workspace::from_root(ctx, &for_task)?;
            Ok(WorkspaceState::open(workspace)?)
        })
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))??;
        let backend = Arc::new(LocalBackend::new(Arc::new(state)));

        // Ends when the backend is dropped and its channel closes: a switch stops it.
        let mut events = backend.subscribe();
        let out = self.events.clone();
        let root = root.to_owned();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let _ = out.send((root.clone(), event));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(backend)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    use super::*;
    use sapphire_workspace::{AppContext, AppKind};

    use crate::test_support;

    static CTX: AppContext = AppContext::new("sapphire-hosttest");

    /// Point the three context directories at `tmp` while holding the crate-wide env
    /// lock, restoring the previous values when dropped — including while unwinding.
    struct EnvGuard {
        previous: [Option<OsString>; 3],
        _lock: MutexGuard<'static, ()>,
    }

    const DIR_VARS: [&str; 3] = [
        "SAPPHIRE_HOSTTEST_CACHE_DIR",
        "SAPPHIRE_HOSTTEST_DATA_DIR",
        "SAPPHIRE_HOSTTEST_CONFIG_DIR",
    ];

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

    fn init_ctx(tmp: &Path) -> EnvGuard {
        let lock = test_support::lock();
        let previous = DIR_VARS.map(std::env::var_os);
        for (name, dir) in DIR_VARS
            .iter()
            .zip(["cache", "data", "config"].map(|c| tmp.join(c)))
        {
            test_support::set(name, &dir);
        }
        CTX.init(AppKind::Server);
        EnvGuard {
            previous,
            _lock: lock,
        }
    }

    fn workspace(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(root.join(".sapphire-hosttest")).unwrap();
        root.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn without_a_selection_there_is_no_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(tmp.path());
        let host = WorkspaceHost::new(&CTX);
        assert!(matches!(host.backend().await, Err(Error::NoWorkspace)));
    }

    #[tokio::test]
    async fn the_selected_workspace_is_opened_once() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(tmp.path());
        let root = workspace(tmp.path(), "ws");
        let host = WorkspaceHost::new(&CTX);
        host.select(&root, false).await.unwrap();
        let a = host.backend().await.unwrap();
        let b = host.backend().await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(host.root(), Some(root));
    }

    #[tokio::test]
    async fn switching_closes_the_old_workspace_and_reselecting_reopens_it() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(tmp.path());
        let a = workspace(tmp.path(), "a");
        let b = workspace(tmp.path(), "b");
        let host = WorkspaceHost::new(&CTX);
        host.select(&a, false).await.unwrap();
        host.backend().await.unwrap();
        host.select(&b, false).await.unwrap();
        assert!(host.open_backend(&a).is_none());
        assert!(matches!(host.backend_at(&a).await, Err(Error::NoWorkspace)));
        // If the switch had left a's database open, this open would fail.
        host.select(&a, false).await.unwrap();
        host.backend().await.unwrap();
    }

    #[tokio::test]
    async fn a_directory_without_a_marker_is_refused_and_the_old_one_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(tmp.path());
        let a = workspace(tmp.path(), "a");
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let host = WorkspaceHost::new(&CTX);
        host.select(&a, false).await.unwrap();
        assert!(host.select(&plain, false).await.is_err());
        assert_eq!(host.root(), Some(a.clone()));
        assert!(host.open_backend(&a).is_some(), "never closed");
    }

    /// Eight callers at once get one backend, not eight opens: an open that overlapped
    /// another of the same workspace would fail with `Database already open`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_uses_are_serialised() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(tmp.path());
        let root = workspace(tmp.path(), "ws");
        let host = Arc::new(WorkspaceHost::new(&CTX));
        host.select(&root, true).await.unwrap();
        host.close();
        host.select(&root, false).await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let host = Arc::clone(&host);
            tasks.push(tokio::spawn(async move { host.backend().await }));
        }
        let mut backends = Vec::new();
        for task in tasks {
            backends.push(task.await.unwrap().unwrap());
        }
        assert!(backends.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])));
    }
}
