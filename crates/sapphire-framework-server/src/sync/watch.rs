//! Noticing edits the app server did not make.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// How long events are coalesced before a root is reported.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Watches workspace roots and reports which one changed.
pub struct Watcher {
    inner: Mutex<notify::RecommendedWatcher>,
    roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl Watcher {
    /// Start watching `roots`, reporting on `tx`.
    pub fn start(roots: Vec<PathBuf>, tx: mpsc::Sender<PathBuf>) -> Result<Watcher> {
        tracing::debug!(?roots, "DIAG Watcher::start");
        let known = Arc::new(Mutex::new(roots.clone()));
        let pending: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

        // The debounce timer: report a root once its last event is DEBOUNCE old.
        {
            let pending = Arc::clone(&pending);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(DEBOUNCE / 3);
                loop {
                    ticker.tick().await;
                    let ready: Vec<PathBuf> = {
                        let mut pending = pending.lock().expect("pending");
                        let ready: Vec<PathBuf> = pending
                            .iter()
                            .filter(|(_, last)| last.elapsed() >= DEBOUNCE)
                            .map(|(root, _)| root.clone())
                            .collect();
                        if !ready.is_empty() {
                            tracing::debug!(?ready, "DIAG debounce READY");
                        }
                        for root in &ready {
                            pending.remove(root);
                        }
                        ready
                    };
                    for root in ready {
                        if tx.send(root).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }

        let handler_roots = Arc::clone(&known);
        let handler_pending = Arc::clone(&pending);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let event = match event { Ok(e) => e, Err(err) => { tracing::debug!("DIAG watch ERR {err}"); return; } };
                tracing::debug!(kind = ?event.kind, paths = ?event.paths, "DIAG watch EVENT");
                let roots = handler_roots.lock().expect("roots").clone();
                for path in event.paths {
                    // Attribute the event to the root it happened under. Nothing is filtered
                    // by path here: what is synced is `SyncFilter`'s decision, made later on
                    // content — the marker directory holds sync-id and config, which sync.
                    if !roots.iter().any(|r| path.starts_with(r)) { tracing::debug!(path = %path.display(), roots = ?roots, "DIAG watch UNATTRIBUTED"); }
                    if let Some(root) = roots.iter().find(|r| path.starts_with(r)) {
                        handler_pending
                            .lock()
                            .expect("pending")
                            .insert(root.clone(), Instant::now());
                    }
                }
            })
            .map_err(|e| Error::Sync(e.to_string()))?;

        for root in &roots {
            // A root that cannot be watched is logged, not fatal: the others must keep
            // working, and the next `scan` will pick this one up anyway.
            if let Err(err) = watcher.watch(root, RecursiveMode::Recursive) {
                tracing::warn!(root = %root.display(), "could not watch: {err}");
            }
        }

        Ok(Watcher {
            inner: Mutex::new(watcher),
            roots: known,
        })
    }

    /// Start watching one more root.
    pub fn watch(&self, root: &Path) -> Result<()> {
        tracing::debug!(root = %root.display(), "DIAG Watcher::watch");
        self.roots.lock().expect("roots").push(root.to_owned());
        self.inner
            .lock()
            .expect("watcher")
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| Error::Sync(e.to_string()))
    }

    /// Stop watching one root.
    pub fn unwatch(&self, root: &Path) {
        self.roots.lock().expect("roots").retain(|r| r != root);
        let _ = self.inner.lock().expect("watcher").unwatch(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn recv(rx: &mut tokio::sync::mpsc::Receiver<PathBuf>) -> Option<PathBuf> {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_created_outside_the_server_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let _watcher = Watcher::start(vec![root.clone()], tx).unwrap();

        std::fs::write(root.join("new.md"), "content").unwrap();
        assert_eq!(recv(&mut rx).await.as_deref(), Some(root.as_path()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_burst_of_writes_produces_one_report() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let _watcher = Watcher::start(vec![root.clone()], tx).unwrap();

        for n in 0..20 {
            std::fs::write(root.join(format!("f{n}.md")), "x").unwrap();
        }
        assert!(recv(&mut rx).await.is_some());

        // Nothing more within another debounce window plus slack.
        let extra = tokio::time::timeout(DEBOUNCE * 3, rx.recv()).await;
        assert!(
            extra.is_err(),
            "the burst should have coalesced, got {extra:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_inside_the_marker_directory_are_reported_too() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(root.join(".test-app")).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let _watcher = Watcher::start(vec![root.clone()], tx).unwrap();

        std::fs::write(root.join(".test-app").join("config.toml"), "x = 1").unwrap();
        assert!(
            recv(&mut rx).await.is_some(),
            "the marker directory holds sync-id and config, which are synced"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_watched_root_that_disappears_does_not_kill_the_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let going = tmp.path().join("going");
        let staying = tmp.path().join("staying");
        std::fs::create_dir_all(&going).unwrap();
        std::fs::create_dir_all(&staying).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let _watcher = Watcher::start(vec![going.clone(), staying.clone()], tx).unwrap();

        std::fs::remove_dir_all(&going).unwrap();
        tokio::time::sleep(DEBOUNCE * 2).await;
        while rx.try_recv().is_ok() {}

        std::fs::write(staying.join("still-here.md"), "x").unwrap();
        assert_eq!(
            recv(&mut rx).await.as_deref(),
            Some(staying.as_path()),
            "one lost root must not stop the others being watched"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_root_added_later_is_watched() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let watcher = Watcher::start(vec![first], tx).unwrap();
        watcher.watch(&second).unwrap();

        std::fs::write(second.join("a.md"), "x").unwrap();
        assert_eq!(recv(&mut rx).await.as_deref(), Some(second.as_path()));
    }
}
