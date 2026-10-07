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

/// The longest a root waits for quiet: one that keeps changing is reported this long
/// after its first unreported event anyway.
pub const MAX_DEBOUNCE: Duration = Duration::from_secs(1);

/// The roots with unreported events, and when each one's events arrived.
///
/// Kept apart from the `notify` callback and the ticker so the decision — which events
/// count, and when a root is due — can be tested with made-up events and instants.
#[derive(Debug, Default)]
struct Pending {
    roots: HashMap<PathBuf, Window>,
}

/// When one root's unreported events began, and when the latest arrived.
#[derive(Debug)]
struct Window {
    first: Instant,
    last: Instant,
}

impl Pending {
    /// Record an event of `kind` under `root` at `now`.
    ///
    /// Access events are dropped: opening or closing a file changes nothing, and inotify
    /// reports every one — including the opens of the scan this report triggers, which
    /// would otherwise schedule the next scan, and the next, for as long as the server
    /// runs. A write shows up as `Create` or `Modify` regardless.
    fn note(&mut self, root: &Path, kind: &notify::EventKind, now: Instant) {
        if matches!(kind, notify::EventKind::Access(_)) {
            return;
        }
        self.roots
            .entry(root.to_owned())
            .and_modify(|window| window.last = now)
            .or_insert(Window {
                first: now,
                last: now,
            });
    }

    /// Take every root that is due at `now`: quiet for [`DEBOUNCE`], or waiting
    /// [`MAX_DEBOUNCE`] since its first event however busy it still is.
    fn take_ready(&mut self, now: Instant) -> Vec<PathBuf> {
        let ready: Vec<PathBuf> = self
            .roots
            .iter()
            .filter(|(_, window)| {
                now.duration_since(window.last) >= DEBOUNCE
                    || now.duration_since(window.first) >= MAX_DEBOUNCE
            })
            .map(|(root, _)| root.clone())
            .collect();
        for root in &ready {
            self.roots.remove(root);
        }
        ready
    }
}

/// Watches workspace roots and reports which one changed.
pub struct Watcher {
    inner: Mutex<notify::RecommendedWatcher>,
    roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl Watcher {
    /// Start watching `roots`, reporting on `tx`.
    pub fn start(roots: Vec<PathBuf>, tx: mpsc::Sender<PathBuf>) -> Result<Watcher> {
        let known = Arc::new(Mutex::new(roots.clone()));
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(Pending::default()));

        // The debounce timer: report a root once it is due (see [`Pending::take_ready`]).
        {
            let pending = Arc::clone(&pending);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(DEBOUNCE / 3);
                loop {
                    ticker.tick().await;
                    let ready = pending.lock().expect("pending").take_ready(Instant::now());
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
                let Ok(event) = event else { return };
                let roots = handler_roots.lock().expect("roots").clone();
                for path in event.paths {
                    // Attribute the event to the root it happened under. Nothing is filtered
                    // by path here: what is synced is `SyncFilter`'s decision, made later on
                    // content — the marker directory holds sync-id and config, which sync.
                    if let Some(root) = roots.iter().find(|r| path.starts_with(r)) {
                        handler_pending.lock().expect("pending").note(
                            root,
                            &event.kind,
                            Instant::now(),
                        );
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

    fn access() -> notify::EventKind {
        notify::EventKind::Access(notify::event::AccessKind::Open(
            notify::event::AccessMode::Any,
        ))
    }

    fn modify() -> notify::EventKind {
        notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ))
    }

    /// Opening a file is not a change. inotify reports every open, and a scan opens
    /// every file: counting those made each scan schedule the next one, so an idle
    /// workspace was rescanned forever (issue #179).
    #[test]
    fn an_access_event_does_not_make_a_root_due() {
        let root = Path::new("/ws");
        let t0 = Instant::now();
        let mut pending = Pending::default();

        pending.note(root, &access(), t0);

        assert!(
            pending.take_ready(t0 + DEBOUNCE * 10).is_empty(),
            "a read must not be reported as an edit"
        );
    }

    /// A steady stream of changes must not postpone the report for ever: the debounce
    /// waits for quiet, but only up to a bound. Without one, a host receiving a peer's
    /// files held back its own edit until the stream stopped (issue #179).
    #[test]
    fn a_steady_stream_of_changes_is_still_reported() {
        let root = Path::new("/ws");
        let t0 = Instant::now();
        let mut pending = Pending::default();
        let step = DEBOUNCE / 2;

        let mut reported_at = None;
        for n in 0..20 {
            let now = t0 + step * n;
            pending.note(root, &modify(), now);
            if !pending.take_ready(now).is_empty() {
                reported_at = Some(now - t0);
                break;
            }
        }
        let reported_at = reported_at.expect("the root was never reported");
        // Checked once per `step`, so the first check at or past the bound may be up to
        // one step late.
        assert!(
            reported_at <= MAX_DEBOUNCE + step,
            "reported only after {reported_at:?}"
        );
    }

    /// The real-filesystem half of [`an_access_event_does_not_make_a_root_due`]. Linux
    /// only: inotify is the backend that reports opens; on Windows and macOS a read raises
    /// no event at all, so the test could not fail there.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn reading_a_file_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.md"), "content").unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let _watcher = Watcher::start(vec![root.clone()], tx).unwrap();
        // Anything the setup itself raised has been reported and drained.
        tokio::time::sleep(DEBOUNCE * 2).await;
        while rx.try_recv().is_ok() {}

        let _ = std::fs::read_to_string(root.join("note.md")).unwrap();
        let reported = tokio::time::timeout(DEBOUNCE * 3, rx.recv()).await;
        assert!(reported.is_err(), "a read was reported: {reported:?}");
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
