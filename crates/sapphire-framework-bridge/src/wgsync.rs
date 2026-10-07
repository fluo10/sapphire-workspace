//! The workgroup's own workspace, replicated like any other workspace.
//!
//! The device ledger and the workspace list live inside the workgroup root, so a pairing on
//! one device reaches the rest by the same replication every other workspace uses. The
//! bridge is the app server that owns this workspace: it scans it, and it serves a
//! replication session to any peer the data plane hands it.
//!
//! Serving is only half of being an owner. The [`spawn_driver`] task is the other half: it
//! watches the workgroup root, records every local change with a scan, and runs a
//! replication session with every peer that has a greater device id — so an admission or a
//! retirement written here reaches the running bridges of the workgroup without anyone
//! doing it by hand.
//!
//! See `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §1: the bridge is
//! the app server of one app, and that app is the workgroup.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use grain_id::GrainId;
use notify::Watcher as _;
use sapphire_framework_session::run_session;
use sapphire_sync::{Replica, ScanOutcome};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc};

use crate::dir::BridgeDir;
use crate::error::{Error, Result};
use crate::peer::PeerTransport;
use crate::workgroup::Workgroup;

/// The application name the workgroup workspace is owned under.
///
/// This is the `app_name` of the routing-table row [`Bridge::run`] registers for the
/// workgroup's own workspace, and the `app_name` of its replica — which is also what puts
/// the `.bridge` marker directory in the workgroup root.
///
/// [`Bridge::run`]: crate::Bridge::run
pub const WORKSPACE_APP_NAME: &str = "bridge";

/// How long a scan waits, at most, for a running session to release the replica.
const SCAN_WAIT: Duration = Duration::from_secs(5);

/// How often a waiting scan re-checks the replica's lock.
const SCAN_POLL: Duration = Duration::from_millis(10);

/// How long the driver waits for a burst of writes to settle before scanning.
///
/// One ledger write is several file events; without a window the driver would scan once per
/// event, and a scan that runs while the next event is still being written may miss it.
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(300);

/// How long a drain waits out the inotify residue of the burst it emptied.
const QUIET: Duration = Duration::from_millis(120);

/// How often the driver sweeps the workgroup workspace even without a local change.
///
/// The same rhythm the app server's dial loop keeps for its workspaces. The sweep is what
/// carries a change to a peer that was offline when it was written — a stale replica's root
/// does not change by itself, so no watch event ever fires for it — and what catches a host
/// that returns after an outage.
pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// How long the longest the driver stays away from a peer that keeps refusing.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// How long one driver-initiated session may run before it is given up on.
///
/// A session between two caught-up replicas is a few frames; one that outlives this is a
/// peer that stopped cooperating, and holding the replica's lock for it would stall every
/// later scan and every inbound session behind it.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the driver waits after the `attempt`-th failure to reach a peer.
///
/// Doubling from one second, capped at [`BACKOFF_MAX`] — the same curve the app server's
/// dial loop applies. The counter resets when a session succeeds, so a peer that is merely
/// restarting is not punished for long.
fn dial_backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.min(9);
    Duration::from_secs(secs).min(BACKOFF_MAX)
}

/// The workgroup's own workspace as a replica.
///
/// The replica's root is the workgroup root (`<bridge dir>/workgroups/<id>/root/`), its
/// store `<bridge dir>/workgroups/<id>/replica/`. One host holds at most one workgroup and
/// therefore at most one of these, opened once by [`Bridge::run`] and kept open for the
/// process's life: the store is a redb database, so a second open while this one lives
/// fails.
///
/// [`Bridge::run`]: crate::Bridge::run
pub struct WorkgroupReplica {
    workspace_id: GrainId,
    /// The replication core. Locked across a whole session — which waits on a peer — so a
    /// `scan` arriving meanwhile takes the lock only briefly or not at all.
    replica: Mutex<Replica>,
}

impl WorkgroupReplica {
    /// Open the workgroup's own replica.
    ///
    /// `device_id` is this host's device record id in the workgroup's ledger — the author
    /// stamped on this replica's local writes. It is taken explicitly because *which* record
    /// is ours is a question a `Workgroup` cannot answer on its own until it knows this
    /// host's node id.
    pub fn open(
        _dir: &BridgeDir,
        workgroup: &Workgroup,
        device_id: GrainId,
    ) -> Result<WorkgroupReplica> {
        // The sync core pauses a workspace whose marker directory is gone, so a root whose
        // files have vanished cannot be read as "everything deleted". The marker belongs to
        // this type: the replica is the thing that syncs the root.
        let root = workgroup.dir.join("root");
        std::fs::create_dir_all(root.join(format!(".{WORKSPACE_APP_NAME}")))?;

        let config = sapphire_sync::ReplicaConfig::new(
            WORKSPACE_APP_NAME,
            root,
            device_id,
            &workgroup.dir.join("replica"),
        );
        let replica = Replica::open(config, Arc::new(sapphire_sync::SystemClock))
            .map_err(|e| Error::Config(format!("the workgroup's replica store: {e}")))?;
        Ok(WorkgroupReplica {
            workspace_id: workgroup.id,
            replica: Mutex::new(replica),
        })
    }

    /// The workgroup id, which doubles as the workspace id of its own workspace.
    pub fn workspace_id(&self) -> GrainId {
        self.workspace_id
    }

    /// Record every unrecorded edit under the workgroup root.
    ///
    /// The replication core is held exclusively across a session — which waits on a peer —
    /// so a scan arriving meanwhile waits up to [`SCAN_WAIT`] for it and then gives up with
    /// an error rather than blocking its caller's thread for the length of a stalled
    /// session. A scan that loses the race is retried by whatever wanted it; nothing is
    /// lost, because a scan only records what is still there.
    pub fn scan(&self) -> Result<()> {
        let outcome = {
            let mut replica = self.lock_for_scan()?;
            replica
                .scan()
                .map_err(|e| Error::Peer(format!("scanning the workgroup workspace: {e}")))?
        };
        match outcome {
            ScanOutcome::Scanned(report) => {
                tracing::debug!(
                    recorded = report.recorded.len(),
                    "scanned the workgroup root"
                );
                Ok(())
            }
            // A paused workgroup root means a user removed the root or its marker while
            // files were materialized. The bridge carries on; the next scan retries.
            ScanOutcome::Paused(reason) => {
                tracing::warn!("the workgroup workspace is paused: {reason:?}");
                Ok(())
            }
        }
    }

    /// Run one replication session over `stream`.
    ///
    /// The session is symmetric: this side sends what the peer lacks and applies what it is
    /// sent, so the same method serves a stream dialed to a peer and one a peer dialed to
    /// the bridge. `run_session` locks the replica itself, only from after the peer's
    /// `Hello` is in through to the end of the exchange — not while waiting for it (issue
    /// #163) — so a peer that merely takes its time with `Hello` does not pin this host's
    /// replica against a concurrent [`scan`](WorkgroupReplica::scan) or another session.
    pub async fn session<S>(&self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let outcome = run_session(stream, &self.replica, self.workspace_id)
            .await
            .map_err(|e| Error::Peer(format!("a workgroup replication session failed: {e}")))?;
        tracing::debug!(?outcome, "a workgroup replication session ended");
        Ok(())
    }

    /// Take the replica's lock without waiting longer than [`SCAN_WAIT`].
    fn lock_for_scan(&self) -> Result<tokio::sync::MutexGuard<'_, Replica>> {
        let deadline = Instant::now() + SCAN_WAIT;
        loop {
            match self.replica.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(_) if Instant::now() >= deadline => {
                    return Err(Error::Peer(
                        "a replication session holds the workgroup's replica".to_owned(),
                    ));
                }
                // A scan is called from sync code; sleeping here is what makes waiting for
                // a session possible without making `scan` async.
                Err(_) => std::thread::sleep(SCAN_POLL),
            }
        }
    }
}

// ── the driver ──────────────────────────────────────────────────────────────

/// Whether an event of `kind` is a change worth a scan.
///
/// Access events are not: opening or closing a file changes nothing, and inotify reports
/// every one — including the opens of the scan a change triggers, which would otherwise
/// schedule another scan for as long as the bridge runs (sapphire-framework#179). A
/// write shows up as `Create` or `Modify` regardless.
fn is_change(kind: &notify::EventKind) -> bool {
    !matches!(kind, notify::EventKind::Access(_))
}

/// Watches the workgroup root and reports when anything under it changed.
///
/// The ledger and the workspace list are written by this host's own control plane
/// (pairing, retirement, publishing) and materialized by inbound replication sessions; the
/// driver scans and dials when either happens. A burst of writes coalesces: the first
/// event starts a [`CHANGE_DEBOUNCE`] window, and whatever arrives during it is drained
/// before the scan runs.
struct ChangeWatch {
    /// Held for its side effects: dropping it stops the watch.
    _watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<()>,
}

impl ChangeWatch {
    /// Watch `root` and everything under it.
    fn start(root: &Path) -> Result<ChangeWatch> {
        let (tx, rx) = mpsc::channel(64);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok_and(|e| is_change(&e.kind)) {
                    // Full is a coalescing miss, not a loss: a later event fires the scan.
                    let _ = tx.try_send(());
                }
            })
            .map_err(|e| Error::Config(format!("watching the workgroup root: {e}")))?;
        watcher
            .watch(root, notify::RecursiveMode::Recursive)
            .map_err(|e| Error::Config(format!("watching the workgroup root: {e}")))?;
        Ok(ChangeWatch {
            _watcher: watcher,
            rx,
        })
    }

    /// The next change, or `None` once the watch has ended.
    async fn changed(&mut self) -> Option<()> {
        self.rx.recv().await
    }

    /// Empty out whatever accumulated behind the last reported change.
    ///
    /// The queue can still hold events the debounce window did not cover, and the kernel
    /// may deliver a write's inotify event a beat after the write returns — including
    /// after this drain. `drain_for` is the caller-facing form: it empties the burst and
    /// then waits out that residue, so what remains is only signals from writes made
    /// after the drain was asked for.
    fn drain(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    /// Drain, then wait out the inotify residue of the burst just emptied.
    ///
    /// `changed` alone would hand back a stale event from before the drain; the driver's
    /// debounce would then scan for a write it has already handled and dial on a change
    /// that is no longer new. Waiting `QUIET` after emptying costs nothing but those few
    /// milliseconds; a signal that arrives during the wait is from a write made after
    /// the drain was asked for, and is left queued for the next round.
    async fn drain_and_settle(&mut self) -> Option<()> {
        self.drain();
        tokio::time::timeout(QUIET, self.changed())
            .await
            .ok()
            .flatten()?; // residue
        self.drain();
        Some(())
    }
}

/// Run the workgroup workspace the way the bridge runs it in production: scan and dial on
/// every change, and sweep every [`SWEEP_INTERVAL`] so a change a peer was offline for
/// reaches it anyway.
///
/// Spawned once per open replica by `Bridge::serve_workgroup`. A re-opened replica (a join
/// replaces the workgroup directory wholesale) replaces the previous task, which the
/// caller aborts.
pub(crate) fn spawn_driver(
    transport: Arc<dyn PeerTransport>,
    workgroup: Workgroup,
    replica: Arc<WorkgroupReplica>,
) -> Result<tokio::task::JoinHandle<()>> {
    let mut watch = ChangeWatch::start(&workgroup.dir.join("root"))?;
    Ok(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        // A missed tick is a late sweep, not a burst of them.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Peers that could not be dialed, and until when they are left alone.
        let mut failures: HashMap<GrainId, u32> = HashMap::new();
        let mut waiting: HashMap<GrainId, Instant> = HashMap::new();

        // The replica was scanned by `serve_workgroup` before this task existed, so what it
        // holds is current. Drive once now: the workgroup may already hold changes nobody
        // dialed for, and the first sweep tick would otherwise be an interval late.
        drive(
            transport.as_ref(),
            &workgroup,
            &replica,
            &mut failures,
            &mut waiting,
        )
        .await;

        loop {
            tokio::select! {
                changed = watch.changed() => {
                    if changed.is_none() {
                        return;
                    }
                    // One write is several events; let the burst finish, then drop it.
                    tokio::time::sleep(CHANGE_DEBOUNCE).await;
                    // The inotify residue of a handled write may arrive after this drain —
                    // waiting it out keeps a scan+dial from running on a change that has
                    // already been handled. A write made while settling is real and stays
                    // queued: it is the next round's change.
                    watch.drain_and_settle().await;
                }
                _ = ticker.tick() => {}
            }
            // The sweep's scan is what catches a local change the watcher missed. Logged,
            // not fatal: the next event or sweep retries, and the drive below still pushes
            // whatever the replica already holds.
            match tokio::task::spawn_blocking({
                let replica = Arc::clone(&replica);
                move || replica.scan()
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::warn!("scanning the workgroup root failed: {err}"),
                Err(err) => tracing::warn!("the workgroup scan task failed: {err}"),
            }
            drive(
                transport.as_ref(),
                &workgroup,
                &replica,
                &mut failures,
                &mut waiting,
            )
            .await;
        }
    }))
}

/// Dial every peer of the workgroup with a greater device id and run one session with each.
///
/// Only the smaller device id of a pair dials — the same rule the app server's dial loop
/// applies. Two simultaneous dials would each hold their replica's lock while waiting for a
/// hello the other side cannot send until its own replica frees: the one race a symmetric
/// session cannot settle by itself. The smaller side sweeps, and every session is
/// symmetric, so the greater side's changes reach the smaller one anyway — at worst a
/// sweep late.
///
/// The dial is unconditional — a session between two caught-up replicas is a few frames —
/// because the sweep is what carries a change a peer missed while offline, and a replica
/// with nothing new records nothing a scan could gate the dial on.
///
/// A retired member is dialed all the same. The retirement is a write like any other, and
/// it has to *reach* the device it retired — its own bridge must learn it is out, or it
/// keeps showing up connected and serving its workspaces. Filtering the dial list here
/// would strand that write on the retiring host, which has by definition no other member
/// greater than itself to carry it. Letting a session through is not letting the device
/// back in: authorization is the gatekeeper for every real ask, and the inbound session is
/// served with only whatever the replica holds — the retired record itself.
async fn drive(
    transport: &dyn PeerTransport,
    workgroup: &Workgroup,
    replica: &WorkgroupReplica,
    failures: &mut HashMap<GrainId, u32>,
    waiting: &mut HashMap<GrainId, Instant>,
) {
    let devices = match workgroup.devices() {
        Ok(devices) => devices,
        Err(err) => {
            tracing::warn!("the workgroup ledger could not be read: {err}");
            return;
        }
    };
    let me = match workgroup.this_device(&transport.node_id()) {
        Ok(me) => me,
        Err(err) => {
            tracing::warn!("this host's own device record: {err}");
            return;
        }
    };
    let now = Instant::now();
    for device in devices.entries() {
        if device.id == me.id {
            continue;
        }
        // Only greater ids dial, so two live hosts never dial each other at once. A retired
        // member is the exception, on either side of that order: the retirement is a write
        // like any other, and it has to *reach* the device it retired — whose id may sort
        // anywhere.
        if device.id < me.id && !device.is_retired() {
            continue;
        }
        // A record written before the device announced itself has no node id, and a peer
        // that cannot be named cannot be dialed.
        let Some(node_id) = device.node_id.as_deref() else {
            continue;
        };
        if waiting.get(&device.id).is_some_and(|until| now < *until) {
            continue;
        }
        let stream = match transport.open(node_id, workgroup.id).await {
            Ok(stream) => stream,
            Err(err) => {
                tracing::debug!(
                    peer = %device.name,
                    "the workgroup workspace could not be dialed: {err}"
                );
                hold_back(failures, waiting, device.id, now);
                continue;
            }
        };
        match tokio::time::timeout(SESSION_TIMEOUT, replica.session(stream)).await {
            Ok(Ok(())) => {
                failures.remove(&device.id);
                waiting.remove(&device.id);
            }
            Ok(Err(err)) => {
                tracing::debug!(peer = %device.name, "a workgroup session failed: {err}");
                hold_back(failures, waiting, device.id, now);
            }
            Err(_) => {
                tracing::warn!(
                    peer = %device.name,
                    "a workgroup session ran over {SESSION_TIMEOUT:?}"
                );
                hold_back(failures, waiting, device.id, now);
            }
        }
    }
}

/// Leave `device` alone for a while, doubling with each consecutive failure.
fn hold_back(
    failures: &mut HashMap<GrainId, u32>,
    waiting: &mut HashMap<GrainId, Instant>,
    device: GrainId,
    now: Instant,
) {
    let count = failures.entry(device).or_insert(0);
    *count = count.saturating_add(1);
    waiting.insert(device, now + dial_backoff(*count));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::BridgeDir;

    const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const NODE_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn host(name: &str, node: &str) -> (tempfile::TempDir, BridgeDir, Workgroup) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        let wg = Workgroup::create(&dir, "test", name, node).unwrap();
        (tmp, dir, wg)
    }

    /// Opening a file is not a change: inotify reports the opens of the very scan a
    /// change triggers, and counting them makes every scan schedule another
    /// (sapphire-framework#179).
    #[test]
    fn an_access_event_is_not_a_change() {
        use notify::event::{AccessKind, AccessMode, DataChange, ModifyKind};
        assert!(!is_change(&notify::EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(is_change(&notify::EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_device_record_replicates_to_the_other_side() {
        let (_ta, dir_a, wg_a) = host("host-a", NODE_A);
        let (_tb, dir_b, _wg_b) = host("host-b", NODE_B);

        // The joiner's own record is admitted on the inviter before the ledger travels: a
        // real pairing writes it there, and `adopt_workgroup` copies the ledger wholesale.
        wg_a.devices()
            .unwrap()
            .add("host-b", Some(NODE_B.to_owned()), None)
            .unwrap();

        // Force both sides onto the same workgroup id, as a real join would.
        let wg_b = crate::testing::adopt_workgroup(&dir_b, &wg_a).unwrap();

        let a =
            WorkgroupReplica::open(&dir_a, &wg_a, wg_a.this_device(NODE_A).unwrap().id).unwrap();
        let b =
            WorkgroupReplica::open(&dir_b, &wg_b, wg_b.this_device(NODE_B).unwrap().id).unwrap();

        a.scan().unwrap();

        let (left, right) = tokio::io::duplex(64 * 1024);
        let (x, y) = tokio::join!(a.session(left), b.session(right));
        x.unwrap();
        y.unwrap();

        assert!(
            wg_b.devices().unwrap().by_node_id(NODE_B).is_some(),
            "B must learn its own record from A"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retirement_replicates() {
        let (_ta, dir_a, wg_a) = host("host-a", NODE_A);
        let (_tb, dir_b, _) = host("host-b", NODE_B);

        // B's own record is in the ledger before it travels: a real pairing writes it
        // there, and `adopt_workgroup` copies the founder's ledger wholesale.
        wg_a.devices()
            .unwrap()
            .add("host-b", Some(NODE_B.to_owned()), None)
            .unwrap();
        let wg_b = crate::testing::adopt_workgroup(&dir_b, &wg_a).unwrap();

        // The phone under test.
        wg_a.devices()
            .unwrap()
            .add("phone", Some("c1".repeat(32)), None)
            .unwrap();
        let a =
            WorkgroupReplica::open(&dir_a, &wg_a, wg_a.this_device(NODE_A).unwrap().id).unwrap();
        let b =
            WorkgroupReplica::open(&dir_b, &wg_b, wg_b.this_device(NODE_B).unwrap().id).unwrap();
        a.scan().unwrap();
        let (l, r) = tokio::io::duplex(64 * 1024);
        let _ = tokio::join!(a.session(l), b.session(r));
        assert!(
            wg_b.devices()
                .unwrap()
                .by_node_id(&"c1".repeat(32))
                .is_some()
        );

        // A retires the phone.
        wg_a.devices().unwrap().retire("phone").unwrap();
        a.scan().unwrap();
        let (l, r) = tokio::io::duplex(64 * 1024);
        let _ = tokio::join!(a.session(l), b.session(r));

        let devices = wg_b.devices().unwrap();
        let phone = devices
            .by_node_id(&"c1".repeat(32))
            .expect("the record stays");
        assert!(phone.is_retired(), "revocation must reach every device");
    }

    /// One session each way: the dialing side is `a`, the accepting side `b`.
    async fn exchange(a: &WorkgroupReplica, b: &WorkgroupReplica) {
        let (l, r) = tokio::io::duplex(64 * 1024);
        let (x, y) = tokio::join!(a.session(l), b.session(r));
        x.unwrap();
        y.unwrap();
    }

    /// A retired member is dialed whatever its id sorts, on either side of the rule that
    /// keeps two live hosts from dialing each other at once. Filtering the dial list by
    /// id order alone would strand the retirement on the retiring host — which by
    /// definition has no member greater than itself to carry it — and the device it
    /// retired would keep showing up connected until it restarted. The device record
    /// fixtures use ids chosen so the retired one sits below (`a1..` < `b1..`) and above
    /// (`c1..`) the retiree's in one test each.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_retired_member_is_dialed_on_either_side_of_the_id_order() {
        for (retiree_node, own_node) in [(NODE_A, NODE_B), ("c1".repeat(32).as_str(), NODE_A)] {
            let (_ta, dir_a, wg_a) = host("host-a", own_node);
            let (_tb, dir_b, _wg_b) = host("phone", retiree_node);

            // The phone is in the ledger before it travels: a real pairing writes it
            // there, and `adopt_workgroup` copies the founder's ledger wholesale.
            wg_a.devices()
                .unwrap()
                .add("phone", Some(retiree_node.to_owned()), None)
                .unwrap();
            let wg_b = crate::testing::adopt_workgroup(&dir_b, &wg_a).unwrap();

            let a = WorkgroupReplica::open(&dir_a, &wg_a, wg_a.this_device(own_node).unwrap().id)
                .unwrap();
            let b =
                WorkgroupReplica::open(&dir_b, &wg_b, wg_b.this_device(retiree_node).unwrap().id)
                    .unwrap();

            // The ledger reaches the phone first, so both sides hold the record.
            a.scan().unwrap();
            exchange(&a, &b).await;
            assert!(
                wg_b.devices().unwrap().by_node_id(retiree_node).is_some(),
                "the phone must hold its own record before the retirement arrives"
            );

            // The retiree sorts below the retiree-side host here, so a dial-list filtered
            // by id order alone would skip it: this is the side the rule change protects.
            wg_a.devices().unwrap().retire("phone").unwrap();
            a.scan().unwrap();
            exchange(&a, &b).await;
            assert!(
                wg_b.devices()
                    .unwrap()
                    .by_node_id(retiree_node)
                    .unwrap()
                    .is_retired(),
                "the retirement must reach the retired device's own replica"
            );
        }
    }

    #[test]
    fn the_workgroup_id_is_its_own_workspace_id() {
        let (_t, dir, wg) = host("host-a", NODE_A);
        let replica =
            WorkgroupReplica::open(&dir, &wg, wg.this_device(NODE_A).unwrap().id).unwrap();
        assert_eq!(replica.workspace_id(), wg.id);
    }

    #[test]
    fn opening_twice_against_one_directory_fails() {
        let (_t, dir, wg) = host("host-a", NODE_A);
        let device = wg.this_device(NODE_A).unwrap().id;
        let _first = WorkgroupReplica::open(&dir, &wg, device).unwrap();
        assert!(
            WorkgroupReplica::open(&dir, &wg, device).is_err(),
            "the replica store is a redb database; a second open must fail loudly"
        );
    }

    #[test]
    fn backoff_doubles_and_stops_at_the_cap() {
        assert_eq!(dial_backoff(0), Duration::from_secs(1));
        assert_eq!(dial_backoff(1), Duration::from_secs(2));
        assert_eq!(dial_backoff(2), Duration::from_secs(4));
        assert_eq!(
            dial_backoff(9),
            BACKOFF_MAX,
            "the cap is what a peer that stays down converges to"
        );
    }

    /// How long one settle may run before "still trickling" is accepted as its answer.
    const SETTLE: Duration = Duration::from_millis(600);

    /// How long the watch as a whole may keep signalling after the one write.
    const QUIET_WATCH: Duration = Duration::from_secs(10);

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_under_the_workgroup_root_is_reported() {
        let (_t, _dir, wg) = host("host-a", NODE_A);
        let mut watch = ChangeWatch::start(&wg.dir.join("root")).unwrap();

        std::fs::write(
            wg.dir.join("root").join("workgroup.toml"),
            "name = \"renamed\"\n",
        )
        .unwrap();
        let reported = tokio::time::timeout(std::time::Duration::from_secs(5), watch.changed())
            .await
            .ok()
            .flatten();
        assert!(
            reported.is_some(),
            "a change under the root must be reported"
        );

        // The write is over, and no second one happened, so nothing new may be
        // reported. What the kernel does with the one write is looser than a single
        // settle can demand: its inotify events may still be trickling in, and one more
        // than the drain covered is a late residue of the same write — or one spurious
        // extra event for it. Both hand the driver a redundant scan+dial, which is
        // idempotent, so neither is a report of a second write. The one thing that must
        // still hold is that the watch goes quiet within a bounded deadline: settle
        // until it does, however much residue comes through.
        let deadline = Instant::now() + QUIET_WATCH;
        loop {
            match tokio::time::timeout(SETTLE, watch.drain_and_settle()).await {
                // Quiet (`None`), or still trickling when `SETTLE` ran out: both are
                // "no second write happened".
                Ok(None) | Err(_) => break,
                Ok(Some(())) if Instant::now() >= deadline => {
                    panic!("the watch never went quiet, and nothing more was written");
                }
                // Residue of the write just handled. Settle again.
                Ok(Some(())) => {}
            }
        }
    }
}
