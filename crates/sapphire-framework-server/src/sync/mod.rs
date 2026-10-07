//! Replication, from the app server's side.
//!
//! One `Replica` per synced workspace, registered with the bridge, driven by announcements
//! from it and by local edits.
//!
//! See `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §4.2.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grain_id::GrainId;
use sapphire_bridge_api::{
    BridgeClient, ManagedBy, RegisterParams, WorkspaceRegistration, WorkspacesResult,
};
use sapphire_sync::{PathUpdate, PauseReason, Replica, ReplicaConfig, ScanOutcome, SystemClock};
use sapphire_workspace::{AppContext, Workspace};
use tokio::sync::{Mutex, OnceCell, broadcast, mpsc};

pub mod id;
mod live;
mod methods;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;
mod watch;

pub use id::{SYNC_ID_FILE, WORKSPACE_MAP_FILE, sync_id, sync_id_path};
pub use live::DIAL_BACKOFF_MAX;
pub use methods::sync_router;
pub use watch::{DEBOUNCE, Watcher};

use live::{LivePeers, dial_backoff};

use crate::error::{Error, Result};

use crate::host::WorkspaceHost;

/// What `sync.status` reports.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct SyncStatus {
    /// Whether this workspace is synced at all.
    pub enabled: bool,
    /// Its identity across devices, when it is.
    pub workspace_id: Option<GrainId>,
    /// How many devices the workgroup has, this host excluded.
    pub peers: usize,
    /// Why replication is paused, if it is — a missing root or marker directory.
    pub paused: Option<String>,
    /// The last failure, if any.
    pub last_error: Option<String>,
    /// Whether the bridge is reachable. `false` is not an outage: the app server works.
    pub bridge_available: bool,
}

/// One workspace this runtime syncs.
struct Synced {
    /// Its identity across devices, shared with every peer.
    workspace_id: GrainId,
    /// Its replica store. Held open because the store is a redb database.
    replica: Arc<Mutex<Replica>>,
    /// Why replication is paused, from the last scan.
    paused: Option<PauseReason>,
    /// The last failure, if any.
    last_error: Option<String>,
}

/// Replication for one application's workspaces.
pub struct SyncRuntime {
    ctx: &'static AppContext,
    bridge: Arc<BridgeClient>,
    exe_path: PathBuf,
    managed_by: ManagedBy,
    synced: Mutex<HashMap<PathBuf, Synced>>,
    /// This host's device id. Asked of the bridge once, on the first [`enable`], and never
    /// again: a second registration with an empty workspace list would briefly clear this
    /// application's routes.
    ///
    /// [`enable`]: SyncRuntime::enable
    device_id: OnceCell<GrainId>,
    /// The file watcher, once [`watch_changes`] has started it.
    ///
    /// The runtime owns it because the spec puts it here: the sync runtime is a watcher and
    /// a replica per registered root, and the two must agree on which roots are synced.
    /// [`enable`] and [`disable`] keep them in step.
    ///
    /// [`watch_changes`]: SyncRuntime::watch_changes
    /// [`enable`]: SyncRuntime::enable
    /// [`disable`]: SyncRuntime::disable
    watcher: OnceCell<Arc<Watcher>>,
    /// The app server that owns these workspaces' caches, once it has told us.
    ///
    /// A replication session writes files, and the search index is not part of a session: the
    /// index belongs to the app server, which holds the workspace open. Reaching it through
    /// the host is what keeps a received file and its index moving together, and it is the
    /// *same* [`WorkspaceState`](sapphire_workspace::WorkspaceState) the server serves from —
    /// opening a second one would collide on the retrieve store's exclusive lock.
    ///
    /// Absent for a runtime nobody has wired to an app server (a test stub driving the
    /// replica directly). Nothing then indexes, which is correct: nothing owns an index.
    host: OnceCell<Arc<WorkspaceHost>>,
    /// Serialises re-indexing, so two sessions finishing at once do not both sweep the
    /// workspace through the same store.
    reindexing: Mutex<()>,
    /// The live session table of every synced workspace, keyed by canonical root.
    ///
    /// One table per workspace rather than one for the runtime: a session is about one
    /// workspace, and the two storm rules are per-workspace rules. A root that is not
    /// synced has none, so a dial or a push for it is a no-op rather than an error.
    live: Mutex<HashMap<PathBuf, Arc<LivePeers>>>,
}

impl SyncRuntime {
    /// A runtime for `ctx`'s application.
    pub fn new(
        ctx: &'static AppContext,
        bridge: Arc<BridgeClient>,
        exe_path: PathBuf,
        managed_by: ManagedBy,
    ) -> SyncRuntime {
        SyncRuntime {
            ctx,
            bridge,
            exe_path,
            managed_by,
            synced: Mutex::new(HashMap::new()),
            device_id: OnceCell::new(),
            watcher: OnceCell::new(),
            host: OnceCell::new(),
            reindexing: Mutex::new(()),
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Tell the runtime which app server owns these workspaces' caches.
    ///
    /// Called by [`AppServer::sync`](crate::AppServer::sync). First writer wins: a runtime
    /// belongs to the server that wired it.
    pub(crate) fn set_host(&self, host: Arc<WorkspaceHost>) {
        let _ = self.host.set(host);
    }

    /// Start syncing `root`, returning its identity across devices.
    ///
    /// Idempotent: enabling an already-synced workspace returns the same id and changes
    /// nothing.
    pub async fn enable(self: &Arc<Self>, root: &Path) -> Result<GrainId> {
        let key = root.canonicalize().map_err(Error::Io)?;
        let watch_key = key.clone();
        let workspace_id = sync_id(self.ctx.app_name, &key)?;

        // The map stays locked across opening the replica, so two simultaneous `enable`s for
        // one root cannot both open it — the store is a redb database, and it is opened once.
        {
            let mut synced = self.synced.lock().await;
            if let Some(existing) = synced.get(&key) {
                return Ok(existing.workspace_id);
            }
            let device_id = self.device_id().await?;
            let state_dir = self.state_dir(&key)?;
            std::fs::create_dir_all(&state_dir).map_err(Error::Io)?;
            let config = ReplicaConfig::new(self.ctx.app_name, key.clone(), device_id, &state_dir);
            let replica = Replica::open(config, Arc::new(SystemClock))
                .map_err(|e| Error::Sync(e.to_string()))?;
            synced.insert(
                key.clone(),
                Synced {
                    workspace_id,
                    replica: Arc::new(Mutex::new(replica)),
                    paused: None,
                    last_error: None,
                },
            );
        }

        // Watch the new root too, so an edit made outside the server from now on is seen.
        // Not fatal if it fails: the app server's own writes take the exact path in
        // `handlers.rs`, and the next `watch_changes` re-reads the root set anyway.
        if let Some(watcher) = self.watcher.get()
            && let Err(err) = watcher.watch(&watch_key)
        {
            tracing::warn!(root = %watch_key.display(), "could not watch: {err}");
        }

        // Register only once the replica is open, so a registration never names a workspace
        // that cannot serve a session.
        self.reregister().await?;

        // Dial now, so enabling a workspace converges it rather than waiting for the next
        // local edit to trigger a session. This is what catches a host up when it returns:
        // the peer that has been running is the one the returning host has to ask, and no
        // watcher on either side fires for a file that was never written locally.
        //
        // Best-effort: a workspace is enabled whether or not any peer is up, and a failure
        // here is not a failure to enable. A session is symmetric, so dialing is enough —
        // this side sends its own view, and the peer answers with what this side lacks.
        if let Err(err) = self.sync_now(&key).await {
            tracing::warn!(root = %key.display(), "the first session after enabling failed: {err}");
        }
        Ok(workspace_id)
    }

    /// Stop syncing `root`. Files and the sync id stay, so re-enabling rejoins the same
    /// workspace rather than creating a second one.
    pub async fn disable(&self, root: &Path) -> Result<()> {
        let Ok(key) = root.canonicalize() else {
            return Ok(());
        };
        let removed = self.synced.lock().await.remove(&key);
        if removed.is_some()
            && let Some(watcher) = self.watcher.get()
        {
            watcher.unwatch(&key);
        }
        // Any live session for a workspace this server no longer holds is closed: it would
        // otherwise keep pushing files for a workspace nobody asked about any more.
        if let Some(peers) = self.live.lock().await.remove(&key) {
            peers.drop_connections().await;
        }
        if let Some(removed) = removed {
            self.bridge
                .unregister(removed.workspace_id)
                .await
                .map_err(|e| Error::Bridge(e.to_string()))?;
            self.reregister().await?;
        }
        Ok(())
    }

    /// The workspaces the workgroup knows about.
    ///
    /// Read straight from the bridge, so it reflects every device's registrations. The
    /// bridge being down is not an empty answer — that would read as "nothing to map" — so
    /// it is an error, and `sync.map` reports it.
    pub async fn workspaces(&self) -> Result<WorkspacesResult> {
        self.bridge
            .workspaces()
            .await
            .map_err(|e| Error::Bridge(e.to_string()))
    }

    /// Map a workgroup workspace onto a directory of this host, and sync it.
    ///
    /// `selector` is the workspace's name or id, as [`workspaces`] lists it; `dir` must
    /// already be a workspace of this application — the directory is placed, not made.
    /// Writing the map and enabling sync are one step: a path mapped but not enabled would
    /// converge on the next start anyway, but a workspace believed synced with no id to
    /// converge under is the state the sync id's error discipline exists to prevent.
    ///
    /// [`workspaces`]: SyncRuntime::workspaces
    pub async fn map(self: &Arc<Self>, selector: &str, dir: &Path) -> Result<GrainId> {
        let workgroup = self.workspaces().await?;
        let wanted = workgroup
            .workspaces
            .iter()
            // A name is the user-facing handle; the id is the exact one. Two workspaces
            // may not share a name, so a name names at most one.
            .find(|w| w.name == selector || w.workspace_id.to_string() == selector)
            .ok_or_else(|| Error::UnknownWorkspaceName(selector.to_owned()))?;

        // Refuse another application's workspace: this server syncs its own application's
        // workspaces, and `Workspace::from_root` would later refuse the directory anyway.
        if wanted.app_name != self.ctx.app_name {
            return Err(Error::WrongApp {
                name: wanted.name.clone(),
                app_name: wanted.app_name.clone(),
            });
        }

        let root = dir.canonicalize().map_err(Error::Io)?;
        let workspace = Workspace::from_root(self.ctx, &root)?;
        if workspace.root != root {
            return Err(Error::UnknownWorkspace(root, self.ctx.app_name));
        }

        // A directory already syncing under another identity is not remapped: the sync id
        // is a device's word that it is the same workspace, and rewriting it silently
        // would make two hosts disagree about what they share.
        {
            let synced = self.synced.lock().await;
            if let Some(entry) = synced.get(&root)
                && entry.workspace_id != wanted.workspace_id
            {
                return Err(Error::SyncId(format!(
                    "{} already syncs as {}, not {}",
                    root.display(),
                    entry.workspace_id,
                    wanted.workspace_id
                )));
            }
        }

        // The identity arrives from the workgroup, not from this host: two hosts that map
        // the same workspace must sync as one, so the id is the one the workgroup lists,
        // written before `enable` reads it. A different id already on disk is refused
        // above; an absent one is written here.
        let marker = workspace.marker_dir();
        let id_path = marker.join(crate::sync::id::SYNC_ID_FILE);
        match std::fs::read_to_string(&id_path) {
            Ok(existing) if existing.trim() != wanted.workspace_id.to_string() => {
                return Err(Error::SyncId(format!(
                    "{} already holds identity {}, not {}",
                    root.display(),
                    existing.trim(),
                    wanted.workspace_id
                )));
            }
            Err(_) => {
                std::fs::write(&id_path, format!("{}\n", wanted.workspace_id))
                    .map_err(Error::Io)?;
            }
            Ok(_) => {}
        }

        // The map is what tells this application, on a later start, which workgroup
        // workspace a directory is.
        let map_path = root
            .join(format!(".{}", self.ctx.app_name))
            .join(WORKSPACE_MAP_FILE);
        let workspace_id = wanted.workspace_id.to_string();
        std::fs::write(&map_path, format!("{workspace_id}\n")).map_err(Error::Io)?;

        self.enable(&root).await
    }

    /// What `sync.status` answers with.
    pub async fn status(&self, root: &Path) -> SyncStatus {
        let bridge_available = self.bridge.status().await.is_ok();
        let peers = match self.bridge.peers().await {
            Ok(p) => p.peers.len().saturating_sub(1),
            Err(_) => 0,
        };
        let Ok(key) = root.canonicalize() else {
            return SyncStatus {
                enabled: false,
                workspace_id: None,
                peers,
                paused: None,
                last_error: None,
                bridge_available,
            };
        };
        let synced = self.synced.lock().await;
        match synced.get(&key) {
            Some(entry) => SyncStatus {
                enabled: true,
                workspace_id: Some(entry.workspace_id),
                peers,
                paused: entry.paused.map(|r| format!("{r:?}")),
                last_error: entry.last_error.clone(),
                bridge_available,
            },
            None => SyncStatus {
                enabled: false,
                workspace_id: None,
                peers,
                paused: None,
                last_error: None,
                bridge_available,
            },
        }
    }

    /// Bring the replica's view of the files up to date.
    ///
    /// Called straight after the app server's own writes, and by the watcher for everything
    /// else. A scan that finds nothing is cheap; a scan that is skipped loses an edit until
    /// the next one.
    pub async fn scan(&self, root: &Path) -> Result<()> {
        let Ok(key) = root.canonicalize() else {
            return Ok(());
        };
        let replica = {
            let synced = self.synced.lock().await;
            match synced.get(&key) {
                Some(entry) => Arc::clone(&entry.replica),
                None => return Ok(()),
            }
        };
        let (outcome, updates) = {
            let mut replica = replica.lock().await;
            let outcome = replica.scan().map_err(|e| Error::Sync(e.to_string()))?;
            // What the scan committed, as path updates, so it can go out on the sessions
            // that are already open. This is the whole point of the exact-path scan in
            // `handlers.rs`: a write through the server reaches a peer live, not at the next
            // dial.
            let updates = match &outcome {
                ScanOutcome::Scanned(report) => {
                    // The unit of replication is the path state (design doc section 2.4):
                    // the `seen` a peer prunes against is the *path's* version vector - the
                    // context the recorded entry was built on plus its own dot - never the
                    // whole replica vv. Sending the global vv here made receivers prune
                    // causally-related versions they still held: a conflict copy on the
                    // first remote edit, and tombstones pruned so deletes stopped
                    // propagating (issue #157).
                    report
                        .recorded
                        .iter()
                        .map(|entry| PathUpdate {
                            path: entry.path.clone(),
                            versions: vec![entry.clone()],
                            seen: {
                                let mut seen = entry.context.clone();
                                seen.add_dot(&entry.dot);
                                seen
                            },
                        })
                        .collect()
                }
                ScanOutcome::Paused(_) => Vec::new(),
            };
            (outcome, updates)
        };
        {
            let mut synced = self.synced.lock().await;
            if let Some(entry) = synced.get_mut(&key) {
                entry.paused = match outcome {
                    ScanOutcome::Paused(reason) => Some(reason),
                    ScanOutcome::Scanned(_) => None,
                };
            }
        }
        // After the map is unlocked: a push awaits every peer, and the table must not be
        // held behind a slow one.
        self.after_commit(&key, updates).await;
        Ok(())
    }

    /// Bring the search index up to date with files a session has written.
    ///
    /// A replication session moves files, not index rows: the index belongs to the app
    /// server, and the sync core is deliberately free of it. So the side that *applied* a
    /// change re-indexes, which is what makes a file that arrived from a peer searchable
    /// without a manual reindex — the claim the whole architecture rests on.
    ///
    /// Incremental, not a full rebuild: the workspace's mtime/size stamps mean only what
    /// actually changed is read back. A file the session wrote has a new stamp, so it is
    /// picked up; one that was already indexed and untouched is skipped.
    ///
    /// Best-effort by contract: the caller's session has already succeeded, and a failure
    /// here must not be reported as the session's. It is logged and the next session, local
    /// edit or explicit `workspace.reindex` tries again.
    async fn reindex(&self, root: &Path) {
        let Some(host) = self.host.get() else {
            return;
        };
        let host = Arc::clone(host);
        let root = root.to_owned();
        // One sweep at a time: two sessions that finish together would otherwise both walk
        // the workspace through the same retrieve store.
        let _serialised = self.reindexing.lock().await;
        // Opening the workspace is itself blocking work, and it is where the exclusive
        // retrieve-store lock is taken, so it happens here rather than inside the sweep.
        let backend = match host.backend(&root).await {
            Ok(backend) => backend,
            Err(err) => {
                tracing::warn!(root = %root.display(), "re-indexing after a session failed: {err}");
                return;
            }
        };
        let state = Arc::clone(backend.state());
        let result = tokio::task::spawn_blocking(move || state.sync_retrieve()).await;
        match result {
            Ok(Ok(report)) => {
                tracing::debug!(root = %root.display(), ?report, "re-indexed after a session");
            }
            Ok(Err(err)) => {
                tracing::warn!(root = %root.display(), "re-indexing after a session failed: {err}");
            }
            Err(err) => {
                tracing::warn!(root = %root.display(), "re-indexing after a session failed: {err}");
            }
        }
    }

    /// Open a session with every peer that will take one.
    pub async fn sync_now(self: &Arc<Self>, root: &Path) -> Result<()> {
        let Ok(key) = root.canonicalize() else {
            return Ok(());
        };
        let workspace_id = {
            let synced = self.synced.lock().await;
            match synced.get(&key) {
                Some(entry) => entry.workspace_id,
                None => return Ok(()),
            }
        };
        let peers = self
            .bridge
            .peers()
            .await
            .map_err(|e| Error::Bridge(e.to_string()))?;
        let me = self.device_id().await?;

        // The same direction rule the dial loop follows: a host only dials peers with a
        // greater device id. Dialling the other way from here would reintroduce the
        // dial-both-ways deadlock the loop was cured of, just on a different trigger.
        for peer in peers.peers.into_iter().filter(|p| me < p.device_id) {
            // A peer that already has a live session is caught up by it: a second exchange
            // on the same workspace would be redundant, and a second stream between the same
            // pair is exactly what the tie-break cannot settle.
            if let Some(live) = self.live_peers(&key).await
                && live.has(&peer.device_id).await
            {
                continue;
            }
            // One dial per peer at a time. `sync_now` runs beside the dial loop, and two
            // overlapping outbound dials to one peer are the one case the tie-break rule
            // cannot resolve — each end would see its own stream as `outbound`. Whoever
            // arrives second simply leaves it to the next pass. The table is made here if
            // the dial loop has not yet: the first dial after enabling must not depend on
            // a tick that has not fired.
            let Some((_, table)) = self.live_peers_or_create(&key).await else {
                continue;
            };
            if !table.begin_dial(peer.device_id).await {
                continue;
            }
            // A peer that does not host this workspace refuses, which is normal and cheap.
            let result = self
                .sync_one(&key, &peer.device_id, peer.name.clone(), workspace_id)
                .await;
            table.end_dial(peer.device_id).await;
            if let Err(err) = result {
                tracing::debug!(peer = %peer.name, "no session: {err}");
            }
        }
        Ok(())
    }

    /// Dial one peer and adopt the stream as a live session.
    ///
    /// The one-shot path (`run_session`) would close the stream the moment the exchange
    /// finished — while the accepting peer keeps its half open and pushes into it, a session
    /// that dies with nothing reading it. So the dialled side is adopted live too, and both
    /// ends of a pair agree that a session stays open.
    async fn sync_one(
        self: &Arc<Self>,
        key: &Path,
        device: &GrainId,
        name: String,
        workspace_id: GrainId,
    ) -> Result<()> {
        let stream = self.bridge.open_stream(workspace_id, *device).await?;
        self.adopt_live_session(stream, key, *device, workspace_id)
            .await
            .map_err(|err| {
                tracing::warn!(peer = %name, "a live session could not be started: {err}");
                err
            })
    }

    /// Answer the bridge's announcements until the connection closes.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mut incoming = self.bridge.incoming();
        loop {
            let announcement = match incoming.recv().await {
                Ok(a) => a,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "fell behind on bridge announcements");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            };

            let found = {
                let synced = self.synced.lock().await;
                synced
                    .iter()
                    .find(|(_, s)| s.workspace_id == announcement.workspace_id)
                    .map(|(root, s)| (root.clone(), Arc::clone(&s.replica)))
            };
            // `replica` is still what the accepted session shares, and `root` is where its
            // files and its index live.
            let Some((root, replica)) = found else {
                // The bridge routed to us for a workspace we no longer hold. Not fatal:
                // ignore it and let the ticket expire.
                tracing::debug!(
                    workspace = %announcement.workspace_id,
                    "an announcement for a workspace this server does not hold"
                );
                continue;
            };

            let bridge = Arc::clone(&self.bridge);
            let workspace_id = announcement.workspace_id;
            let peer = announcement.peer_device_id;
            let driver = Arc::clone(&self);
            tokio::spawn(async move {
                let stream = match bridge.accept_stream(announcement.ticket).await {
                    Ok(stream) => stream,
                    Err(err) => {
                        tracing::warn!("could not claim an announced stream: {err}");
                        return;
                    }
                };
                // Live, not one-shot: the peer that dialled is keeping this session open and
                // pushing what it commits, so this side has to keep reading. A session that
                // closed after the exchange would leave the dialler pushing into a stream
                // nobody reads.
                let (_, session) = match sapphire_framework_session::open_live_session(
                    stream,
                    Arc::clone(&replica),
                    workspace_id,
                )
                .await
                {
                    Ok(started) => started,
                    Err(err) => {
                        tracing::warn!("an inbound session failed: {err}");
                        return;
                    }
                };
                let Some((key, table)) = driver.live_peers_or_create(&root).await else {
                    return;
                };
                // The peer dialled: this stream survives only if the *peer* has the lower
                // device id. A stream that loses is dropped, which closes it, and the
                // connection ends tidily rather than being left half-read.
                let Some(updates) = table.insert(peer, session, false).await else {
                    return;
                };
                // This is the receiving side: the files are on disk now and nobody else will
                // index them.
                driver.reindex(&key).await;
                driver.spawn_reader(table, updates, key, peer);
            });
        }
    }

    /// Every synced root, for the watcher.
    pub async fn roots(&self) -> Vec<PathBuf> {
        self.synced.lock().await.keys().cloned().collect()
    }

    /// The live session table for `root`, if this runtime syncs it.
    ///
    /// `None` is not an error: a dial, a push or a drop for a workspace this server does not
    /// hold is a no-op, and every caller below treats it that way.
    async fn live_peers(&self, root: &Path) -> Option<Arc<LivePeers>> {
        let key = root.canonicalize().ok()?;
        self.live.lock().await.get(&key).cloned()
    }

    /// The live session table for `root`, creating it if this runtime syncs it.
    ///
    /// The two locks are taken one after the other, never nested: `synced` to decide whether
    /// the workspace is ours at all, then `live` to get or make its table.
    async fn live_peers_or_create(&self, root: &Path) -> Option<(PathBuf, Arc<LivePeers>)> {
        let key = root.canonicalize().ok()?;
        if !self.synced.lock().await.contains_key(&key) {
            return None;
        }
        // A table is built by whoever walks the workspace first, and it needs this host's
        // device id — the tie-break between two simultaneous streams is decided from it.
        let me = self.device_id().await.ok()?;
        let mut live = self.live.lock().await;
        let peers = Arc::clone(
            live.entry(key.clone())
                .or_insert_with(|| Arc::new(LivePeers::new(me))),
        );
        Some((key, peers))
    }

    /// Push `updates` to every open session for `root`.
    ///
    /// Called with what a scan recorded. `from` is always `None` here: a local commit came
    /// from this host, so no session is excluded. The forwarding path, which does name a
    /// source, is [`LivePeers::fan_out`](live::LivePeers::fan_out) called directly by the
    /// reader side of a session.
    ///
    /// Best-effort by contract: the commit already happened, and a peer that is gone is the
    /// dialer's problem, not the writer's.
    pub async fn after_commit(&self, root: &Path, updates: Vec<PathUpdate>) {
        if updates.is_empty() {
            return;
        }
        if let Some(peers) = self.live_peers(root).await {
            peers.fan_out(&updates, None).await;
        }
    }

    /// The devices with an open live session for `root`.
    ///
    /// Used by tests to wait for the dialer; empty for a workspace this server does not hold.
    pub async fn live_session_devices(&self, root: &Path) -> Vec<GrainId> {
        match self.live_peers(root).await {
            Some(peers) => peers.devices().await,
            None => Vec::new(),
        }
    }

    /// Close every open live session, as if the connections had been cut.
    ///
    /// Called for a workspace being disabled, and by tests that want to watch the dialer
    /// rebuild what was dropped.
    pub async fn drop_connections(&self) {
        let tables: Vec<Arc<LivePeers>> = self.live.lock().await.values().cloned().collect();
        for peers in tables {
            peers.drop_connections().await;
        }
    }

    /// Keep a live session open to every peer, for every synced workspace.
    ///
    /// One walk per [`DIAL_INTERVAL`](live::DIAL_INTERVAL): for each synced workspace, ask
    /// the bridge who is in the workgroup, and open a session to every peer that is connected
    /// and does not already have one.
    ///
    /// A peer that cannot be reached is retried on a per-peer exponential backoff capped at
    /// [`DIAL_BACKOFF_MAX`] — one failed attempt per walk, not one per spin — and the counter
    /// resets when a session opens. An unreachable peer is therefore no obstacle to the
    /// others: its dial fails, the walk moves on, and the next workspace is dialled.
    ///
    /// Runs until the task is aborted, which is what the app server's shutdown does with it.
    /// It is allowed to fail like [`run`](SyncRuntime::run): only sync stops, and the app
    /// server keeps serving files (spec §10).
    pub async fn dial_loop(self: Arc<Self>) -> Result<()> {
        let mut tick = tokio::time::interval(live::DIAL_INTERVAL);
        // A missed tick is a late walk, not a burst of them: the interval is a pace, not a
        // quota to make up.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut waiting: HashMap<GrainId, std::time::Instant> = HashMap::new();
        let mut failures: HashMap<GrainId, u32> = HashMap::new();
        loop {
            tick.tick().await;
            let roots = self.roots().await;
            if roots.is_empty() {
                continue;
            }
            let peers = match self.bridge.peers().await {
                Ok(peers) => peers,
                // The bridge is down or has no workgroup: nothing to dial, and the next walk
                // asks again. Not an error for the caller — the app server is still serving.
                Err(err) => {
                    tracing::debug!("no peers to dial: {err}");
                    continue;
                }
            };
            let me = match self.device_id().await {
                Ok(me) => me,
                Err(err) => {
                    tracing::warn!("no device id, so nobody to dial: {err}");
                    continue;
                }
            };
            let now = std::time::Instant::now();
            // Dial only peers whose device id is greater than ours. Every device dialling
            // every other can deadlock: two hosts that dial each other at once each hold
            // their replica's lock across their own exchange and wait for the other's
            // hello, which the other's dialler holds the lock for. With one direction per
            // pair the wait chain is a DAG — a higher-id host never dials, so it never
            // holds its lock out while a lower-id host's exchange needs it. The lower host
            // dials within `DIAL_INTERVAL`, and the exchange is symmetric, so nothing is
            // lost; the tie-break is left as the safety net it is.
            for root in roots {
                let Some((key, table)) = self.live_peers_or_create(&root).await else {
                    continue;
                };
                let workspace_id = {
                    let synced = self.synced.lock().await;
                    match synced.get(&key) {
                        Some(entry) => entry.workspace_id,
                        None => continue,
                    }
                };
                for peer in peers
                    .peers
                    .iter()
                    .filter(|p| p.connected && me < p.device_id)
                {
                    let device = peer.device_id;
                    if table.has(&device).await {
                        // Already talking; a redial would drop a working session.
                        failures.remove(&device);
                        waiting.remove(&device);
                        continue;
                    }
                    if let Some(until) = waiting.get(&device)
                        && now < *until
                    {
                        // Backing off after a failure; the walk skips it until then.
                        continue;
                    }
                    // The same claim `sync_now` takes: this walk and it can overlap, and a
                    // double dial in one direction is the one race the tie-break cannot
                    // settle. Skipping is free — the next pass, or `sync_now` itself, dials.
                    if !table.begin_dial(device).await {
                        continue;
                    }
                    let opened = self.bridge.open_stream(workspace_id, device).await;
                    if opened.is_err() {
                        table.end_dial(device).await;
                    }
                    match opened {
                        Ok(stream) => {
                            let adopted = self
                                .adopt_live_session(stream, &root, device, workspace_id)
                                .await;
                            // The claim is out either way: the attempt is over, and a
                            // session that died mid-exchange is re-dialled next pass.
                            table.end_dial(device).await;
                            match adopted {
                                Ok(()) => {
                                    failures.remove(&device);
                                    waiting.remove(&device);
                                }
                                Err(err) => {
                                    tracing::debug!(
                                        peer = %peer.name,
                                        err = %err,
                                        "a live session could not be started"
                                    );
                                    let count = failures.entry(device).or_insert(0);
                                    *count = count.saturating_add(1);
                                    waiting.insert(device, now + dial_backoff(*count));
                                }
                            }
                        }
                        Err(err) => {
                            // Normal and cheap: a peer that does not host this workspace
                            // refuses, and one that is unreachable cannot be opened.
                            tracing::debug!("no stream to a peer: {err}");
                            let count = failures.entry(device).or_insert(0);
                            *count = count.saturating_add(1);
                            waiting.insert(device, now + dial_backoff(*count));
                        }
                    }
                }
            }
        }
    }

    /// Take ownership of an opened stream: run the exchange, then keep it live.
    ///
    /// The session is stored under `device` and its reader pumped until the stream ends. A
    /// half-open or refused stream is not stored; the dialer retries it with backoff.
    async fn adopt_live_session(
        self: &Arc<Self>,
        stream: sapphire_ipc::RawStream,
        root: &Path,
        device: GrainId,
        workspace_id: GrainId,
    ) -> Result<()> {
        let replica = self.replica_of(root).await?;
        let (_, session) = sapphire_framework_session::open_live_session(
            stream,
            Arc::clone(&replica),
            workspace_id,
        )
        .await
        .map_err(|e| Error::Sync(e.to_string()))?;
        let Some((key, table)) = self.live_peers_or_create(root).await else {
            return Ok(());
        };
        // This stream was dialled by us, so it is the one the pair keeps if this host's
        // device id is the lower of the two. If it lost the tie-break the session is
        // dropped here, which closes the stream, and the surviving one is already in the
        // table.
        let Some(updates) = table.insert(device, session, true).await else {
            return Ok(());
        };
        // The session wrote files during the exchange; the index has to catch up before the
        // workspace is searchable again.
        self.reindex(&key).await;
        self.spawn_reader(Arc::clone(&table), updates, key, device);
        Ok(())
    }

    /// Forward what one session receives, and re-index it.
    ///
    /// This is the loop that makes A → S → B work: a batch that arrived on S's session with A
    /// is applied (the session crate does that before publishing it), the workspace behind
    /// `key` is re-indexed, and the batch is handed to every *other* session whose peer lacks
    /// it — `from` is the device it came from, so it never goes back to A.
    fn spawn_reader(
        self: &Arc<Self>,
        table: Arc<LivePeers>,
        mut updates: broadcast::Receiver<Vec<PathUpdate>>,
        key: PathBuf,
        from: GrainId,
    ) {
        let driver = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match updates.recv().await {
                    Ok(batch) => {
                        // The files are on disk now (the session guarantees it before
                        // publishing), so the index is behind them until the sweep below.
                        driver.reindex(&key).await;
                        table.fan_out(&batch, Some(from)).await;
                    }
                    // A burst bigger than the channel: the newest is kept and the gap is
                    // covered by the next session's exchange. Losing a batch here cannot lose
                    // an entry — the peer's vector does not cover it, so the next dial or push
                    // carries it again.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::debug!(
                            device = %from,
                            missed,
                            "fell behind on a live session; the next exchange catches up"
                        );
                    }
                    // The session closed. Its table entry is reaped by the next fan-out or
                    // dial pass, and the dialer opens a fresh one.
                    Err(broadcast::error::RecvError::Closed) => {
                        return;
                    }
                }
            }
        });
    }

    /// The replica of a synced `root`.
    async fn replica_of(&self, root: &Path) -> Result<Arc<Mutex<Replica>>> {
        let key = root.canonicalize().map_err(Error::Io)?;
        let synced = self.synced.lock().await;
        match synced.get(&key) {
            Some(entry) => Ok(Arc::clone(&entry.replica)),
            None => Err(Error::UnknownWorkspace(key, self.ctx.app_name)),
        }
    }

    /// Watch the synced roots and, on a debounced report, scan and dial.
    ///
    /// This is the safety net for everything the app server did not do itself — a file
    /// edited in an editor, a `git checkout`. The exact path is [`scan`] called from
    /// `handlers.rs` right after a write the server made. A scan that finds nothing is
    /// cheap; a scan that is skipped loses an edit until the next one.
    ///
    /// Returns when the reporting channel closes — that is, when the [`Watcher`] this
    /// runtime holds is dropped, which happens when the runtime itself is.
    ///
    /// [`scan`]: SyncRuntime::scan
    pub async fn watch_changes(self: Arc<Self>) -> Result<()> {
        let (tx, mut rx) = mpsc::channel(64);
        let watcher = Arc::new(Watcher::start(self.roots().await, tx)?);
        let _ = self.watcher.set(Arc::clone(&watcher));

        // A root enabled between `roots()` above and the `set` just now was not in the
        // starting set, and `enable` could not reach a watcher that did not exist yet.
        // Watching the current set again is idempotent on `notify`'s side.
        for root in self.roots().await {
            if let Err(err) = watcher.watch(&root) {
                tracing::warn!(root = %root.display(), "could not watch: {err}");
            }
        }

        while let Some(root) = rx.recv().await {
            // Logged, not fatal: a failed scan or dial leaves the replica behind, and the
            // next report tries again. Neither may take the app server down (spec §10).
            if let Err(err) = self.scan(&root).await {
                tracing::warn!(root = %root.display(), "scan after a local edit failed: {err}");
            }
            if let Err(err) = self.sync_now(&root).await {
                tracing::warn!(root = %root.display(), "dial after a local edit failed: {err}");
            }
        }
        Ok(())
    }

    /// Tell the bridge the complete current set. A registration is not a delta.
    async fn reregister(&self) -> Result<()> {
        let workspaces: Vec<WorkspaceRegistration> = {
            let synced = self.synced.lock().await;
            synced
                .iter()
                .map(|(root, entry)| WorkspaceRegistration {
                    workspace_id: entry.workspace_id,
                    root: root.clone(),
                })
                .collect()
        };
        self.bridge
            .register(RegisterParams {
                app_name: self.ctx.app_name.to_owned(),
                exe_path: self.exe_path.clone(),
                managed_by: self.managed_by,
                workspaces,
            })
            .await
            .map_err(|e| Error::Bridge(e.to_string()))?;
        Ok(())
    }

    /// This host's device id, asked of the bridge once.
    async fn device_id(&self) -> Result<GrainId> {
        if let Some(id) = self.device_id.get() {
            return Ok(*id);
        }
        let result = self
            .bridge
            .register(RegisterParams {
                app_name: self.ctx.app_name.to_owned(),
                exe_path: self.exe_path.clone(),
                managed_by: self.managed_by,
                workspaces: Vec::new(),
            })
            .await
            .map_err(|e| Error::Bridge(e.to_string()))?;
        let _ = self.device_id.set(result.device_id);
        Ok(result.device_id)
    }

    /// `<workspace cache dir>/sync/`.
    fn state_dir(&self, root: &Path) -> Result<PathBuf> {
        let workspace = Workspace::from_root(self.ctx, root)?;
        Ok(workspace.cache_dir().join("sync"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::testing::StubBridge;
    use crate::test_support;
    use sapphire_workspace::AppKind;
    use std::ffi::OsString;

    static CTX: AppContext = AppContext::new("sapphire-synctest");

    /// The env vars `CTX.init` reads.
    const DIR_VARS: [&str; 3] = [
        "SAPPHIRE_SYNCTEST_CACHE_DIR",
        "SAPPHIRE_SYNCTEST_DATA_DIR",
        "SAPPHIRE_SYNCTEST_CONFIG_DIR",
    ];

    /// Points the context's directories at the test's scratch tree, and restores the
    /// previous values when dropped — including while unwinding from a panic.
    struct EnvGuard {
        previous: [Option<OsString>; 3],
        _lock: std::sync::MutexGuard<'static, ()>,
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

    /// Everything one test needs, held for the test's whole body.
    ///
    /// The context is a `static` shared by this binary, so its directories are resolved from
    /// the environment by whichever test initialises it first. The environment lock is
    /// therefore held for the whole test, as in the crate's other test modules, and `_tmp`
    /// is declared before `_env` so the scratch tree is gone only once the environment no
    /// longer points at it.
    struct Fixture {
        _tmp: tempfile::TempDir,
        _env: EnvGuard,
        root: PathBuf,
        stub: StubBridge,
        runtime: Arc<SyncRuntime>,
    }

    impl Fixture {
        /// A second workspace root beside the first, marker directory and all.
        fn second_root(&self) -> PathBuf {
            let root = self._tmp.path().join("ws2");
            std::fs::create_dir_all(root.join(".sapphire-synctest")).unwrap();
            root.canonicalize().unwrap()
        }
    }

    async fn fixture() -> Fixture {
        let lock = test_support::lock();
        let tmp = tempfile::tempdir().unwrap();
        let previous = DIR_VARS.map(std::env::var_os);
        // SAFETY (via `test_support::set`): `lock` serialises every read and write of the
        // process environment in this test binary, and it is held until `drop` has restored
        // the old values.
        for (name, dir) in DIR_VARS
            .iter()
            .zip(["cache", "data", "config"].map(|cat| tmp.path().join(cat)))
        {
            test_support::set(name, &dir);
        }
        let env = EnvGuard {
            previous,
            _lock: lock,
        };
        CTX.init(AppKind::Server);

        let root = tmp.path().join("ws");
        std::fs::create_dir_all(root.join(".sapphire-synctest")).unwrap();
        let root = root.canonicalize().unwrap();

        let (stub, client) = StubBridge::start().await;
        let runtime = Arc::new(SyncRuntime::new(
            &CTX,
            client,
            "/bin/true".into(),
            ManagedBy::Service,
        ));
        Fixture {
            _tmp: tmp,
            _env: env,
            root,
            stub,
            runtime,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn enabling_registers_the_workspace_with_the_bridge() {
        let f = fixture().await;

        let id = f.runtime.enable(&f.root).await.unwrap();
        assert_eq!(f.stub.last_workspaces(), vec![id]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn enabling_twice_is_idempotent_and_keeps_the_same_id() {
        let f = fixture().await;

        let first = f.runtime.enable(&f.root).await.unwrap();
        let second = f.runtime.enable(&f.root).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(
            f.stub.last_workspaces(),
            vec![first],
            "still exactly one workspace"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_registration_carries_the_complete_current_list() {
        let f = fixture().await;
        let second_root = f.second_root();

        let a = f.runtime.enable(&f.root).await.unwrap();
        let b = f.runtime.enable(&second_root).await.unwrap();

        let mut seen = f.stub.last_workspaces();
        seen.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(seen, want, "registration is the whole set, not a delta");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_later_registration_clears_this_apps_routes() {
        // The device id is asked of the bridge once, on the first `enable`, with an empty
        // workspace list — the one moment that is safe, because no route exists yet. Every
        // later registration must name the complete set: one with an empty list would
        // briefly clear this application's routes, and a peer asking for a workspace this
        // server owns would be told nobody has it.
        let f = fixture().await;
        let second_root = f.second_root();

        f.runtime.enable(&f.root).await.unwrap();
        f.runtime.enable(&second_root).await.unwrap();
        f.runtime.sync_now(&f.root).await.unwrap();
        f.runtime.disable(&f.root).await.unwrap();

        let seen = f.stub.seen.lock().expect("stub");
        let first_full = seen
            .registrations
            .iter()
            .position(|r| !r.workspaces.is_empty())
            .expect("enabling registers the workspace");
        let emptied_afterwards = seen.registrations[first_full + 1..]
            .iter()
            .filter(|r| r.workspaces.is_empty())
            .count();
        assert_eq!(
            emptied_afterwards, 0,
            "a registration after the first workspace must still name the whole set"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disabling_unregisters_and_leaves_the_files_alone() {
        let f = fixture().await;
        std::fs::write(f.root.join("keep.md"), "content").unwrap();

        let id = f.runtime.enable(&f.root).await.unwrap();
        f.runtime.disable(&f.root).await.unwrap();

        assert!(f.stub.last_workspaces().is_empty());
        assert!(
            f.root.join("keep.md").exists(),
            "disabling sync must not touch files"
        );
        assert!(
            crate::sync::id::sync_id_path("sapphire-synctest", &f.root).exists(),
            "the sync id stays, so re-enabling rejoins the same workspace"
        );
        let _ = id;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn re_enabling_after_disabling_reuses_the_identity() {
        let f = fixture().await;

        let first = f.runtime.enable(&f.root).await.unwrap();
        f.runtime.disable(&f.root).await.unwrap();
        let again = f.runtime.enable(&f.root).await.unwrap();

        assert_eq!(first, again);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_reports_disabled_for_a_workspace_that_was_never_enabled() {
        let f = fixture().await;

        let status = f.runtime.status(&f.root).await;
        assert!(!status.enabled);
        assert!(status.workspace_id.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disabling_something_that_was_never_enabled_is_not_an_error() {
        let f = fixture().await;
        f.runtime
            .disable(&f.root)
            .await
            .expect("disabling twice is fine");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_workspace_whose_root_vanished_is_reported_as_paused_not_deleted() {
        let f = fixture().await;
        std::fs::write(f.root.join("a.md"), "content").unwrap();
        f.runtime.enable(&f.root).await.unwrap();
        f.runtime.scan(&f.root).await.unwrap();

        // The drive is unmounted: the marker directory goes with it.
        std::fs::remove_dir_all(f.root.join(".sapphire-synctest")).unwrap();
        let _ = f.runtime.scan(&f.root).await;

        let status = f.runtime.status(&f.root).await;
        assert!(
            status.paused.is_some(),
            "a missing root must pause, not replicate as a mass deletion"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_directory_that_is_not_a_workspace_cannot_be_enabled() {
        let f = fixture().await;
        let plain = f._tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        assert!(f.runtime.enable(&plain).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_through_the_workspace_router_is_committed_to_the_replica() {
        // The brief's "main path": a write the app server itself made is scanned straight
        // after, not left to the watcher's debounce. Asserted against the replica's own
        // state, which is the only thing that can tell the two paths apart — reading the
        // file back would pass even if the scan never ran.
        let f = fixture().await;
        f.runtime.enable(&f.root).await.unwrap();

        let host = Arc::new(crate::WorkspaceHost::new(&CTX));
        let router = Arc::new(crate::workspace_router_with_sync(
            host,
            Some(Arc::clone(&f.runtime)),
        ));
        let (client_conn, server_conn) = sapphire_ipc::Connection::pair();
        tokio::spawn(async move {
            let info = sapphire_ipc::ServerInfo {
                version: "0.0.0".into(),
                api: sapphire_backend::protocol::API_VERSION,
                pid: std::process::id(),
                managed_by: ManagedBy::Spawned,
            };
            let _ = sapphire_ipc::serve(server_conn, router, "sapphire-synctest", info).await;
        });
        let info = sapphire_ipc::ClientInfo {
            kind: "test".into(),
            version: "0.0.0".into(),
            api: sapphire_backend::protocol::API_VERSION,
            pid: std::process::id(),
        };
        let (client, _) = sapphire_ipc::Client::handshake(client_conn, "sapphire-synctest", info)
            .await
            .unwrap();

        let _: sapphire_backend::protocol::Ack = client
            .call(
                sapphire_backend::protocol::WRITE_FILE,
                sapphire_backend::protocol::ContentParams {
                    ws: f.root.clone(),
                    path: PathBuf::from("through-the-server.md"),
                    content: "written by the server".into(),
                },
            )
            .await
            .unwrap();

        // `scan` runs before the reply is written, so by the time the call returns the
        // replica must already know the path.
        let synced = f.runtime.synced.lock().await;
        let entry = synced.get(&f.root).expect("enabled above");
        let replica = entry.replica.lock().await;
        let state = replica
            .state("through-the-server.md")
            .expect("a readable state");
        assert!(
            state.is_some(),
            "a write through the server must be scanned into the replica, not left to the watcher"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_announcement_for_an_unknown_workspace_is_ignored_not_fatal() {
        let f = fixture().await;
        f.runtime.enable(&f.root).await.unwrap();

        let driver = Arc::clone(&f.runtime);
        let handle = tokio::spawn(async move { driver.run().await });

        f.stub
            .announce(grain_id::GrainId::random(), "no-such-ticket")
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(
            !handle.is_finished(),
            "one bad announcement must not stop the runtime"
        );
        handle.abort();
    }
}
