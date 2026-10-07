//! The live session tables of a SyncRuntime, one per synced workspace.
//!
//! One `LivePeers` per synced workspace: it holds the open sessions to peers, pushes a local
//! commit to all of them, and forwards what a session receives to every session whose peer
//! lacks it. The dialing loop and the inbound accept path both end here, at `insert`, so the
//! two sides of one pair agree on which stream survives — the one rule that keeps two
//! scheduled dialers from keeping two different streams alive.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use grain_id::GrainId;
use sapphire_framework_session::LiveSession;
use sapphire_sync::PathUpdate;
use tokio::sync::{Mutex, broadcast};

/// How often the dialer walks the peers.
pub const DIAL_INTERVAL: Duration = Duration::from_secs(5);

/// The longest the dialer stays away from a peer that keeps refusing.
pub const DIAL_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// How long the dialer waits after the `attempt`-th failure to a peer.
///
/// Doubling from one second: 1, 2, 4, 8, … capped at five minutes. The counter resets when a
/// session opens, so a peer that is merely slow is not punished for long.
pub(crate) fn dial_backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.min(9);
    Duration::from_secs(secs).min(DIAL_BACKOFF_MAX)
}

/// The live session table of one workspace.
pub(crate) struct LivePeers {
    /// This host's device id, so a pair's two ends can agree on which of their streams
    /// survives — see [`LivePeers::insert`].
    me: GrainId,
    /// One session per peer device, while it is open.
    sessions: Mutex<HashMap<GrainId, Arc<LiveSession>>>,
    /// The peers this host is dialing right now.
    ///
    /// A dial is started from two places — `sync_now` when a workspace is enabled, and the
    /// dial loop on its interval — and they can overlap. Two outbound streams to one peer
    /// would give the pair's two ends a second stream whose fate the one `insert` rule
    /// cannot settle (both ends would see `outbound`), so a dialer claims a peer before
    /// opening its stream and releases the claim when the attempt is over; the second
    /// dialer skips it until the next walk.
    dialing: Mutex<HashSet<GrainId>>,
}

impl LivePeers {
    /// The session table of a host with device id `me`.
    pub(crate) fn new(me: GrainId) -> LivePeers {
        LivePeers {
            me,
            sessions: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashSet::new()),
        }
    }

    /// Claim the right to dial `device`, or `false` if a dial is already under way.
    pub(crate) async fn begin_dial(&self, device: GrainId) -> bool {
        self.dialing.lock().await.insert(device)
    }

    /// Release the claim [`begin_dial`](Self::begin_dial) took.
    pub(crate) async fn end_dial(&self, device: GrainId) {
        self.dialing.lock().await.remove(&device);
    }

    /// Send `updates` to every open session whose peer lacks them.
    ///
    /// `from` is the device a batch arrived from, when forwarding: it is never sent back
    /// there. Two rules keep a triangle quiet:
    ///
    /// 1. never back to the source (`from` is skipped);
    /// 2. never what the peer already has (the session's `peer_vv` covers it).
    ///
    /// A push that fails is not retried here: the session is dead or the peer is gone, the
    /// next fan-out or dial pass notices, and the batch rides the next exchange. One slow
    /// peer does not stall the rest — each push is its own await.
    pub(crate) async fn fan_out(&self, updates: &[PathUpdate], from: Option<GrainId>) {
        if updates.is_empty() {
            return;
        }
        let mut sessions = self.sessions.lock().await;
        // Closed sessions go now: nobody is behind them waiting to be told.
        sessions.retain(|_, session| session.is_open());
        for (device, session) in sessions.iter() {
            if Some(device) == from.as_ref() {
                continue;
            }
            let seen: Vec<_> = updates.iter().map(|u| u.seen.clone()).collect();
            if seen.iter().all(|v| session.peer_vv().covers(v)) {
                tracing::debug!(%device, paths = ?updates.iter().map(|u| &u.path).collect::<Vec<_>>(), seen = ?seen, peer_vv = ?session.peer_vv(), "DIAG fan_out SKIP: peer covers");
                continue;
            }
            tracing::debug!(%device, paths = ?updates.iter().map(|u| &u.path).collect::<Vec<_>>(), seen = ?seen, peer_vv = ?session.peer_vv(), "DIAG fan_out PUSH");
            if let Err(err) = session.push(updates.to_vec()).await {
                tracing::debug!(%device, "a live push failed: {err}");
            }
        }
    }

    /// Remember `session` as the open session to `device`, and return its update stream.
    ///
    /// `outbound` says which side dialled. Both ends of a pair dial on a schedule, so two
    /// streams between the same two devices routinely come up at once — and if each end kept
    /// whichever it stored last, the two ends could keep *different* streams, each one
    /// holding a session whose far half the peer had already dropped. Both would then read
    /// EOF and go quiet, and the pair would never settle.
    ///
    /// So the two ends agree on a rule both can compute: **the stream whose initiator has
    /// the lower device id survives**. For the stream A dialled, A sees `outbound` with
    /// `A < B` — keeps. For the stream B dialled, A sees `!outbound` with `A < B` — drops.
    /// Same stream, same verdict, either end.
    ///
    /// `None` means this stream lost the tie-break: it is dropped here, closing it, and the
    /// surviving one is already in the table. Subscribing *before* the session is stored is
    /// what guarantees the caller's reader sees every batch announced from now on.
    pub(crate) async fn insert(
        &self,
        device: GrainId,
        session: LiveSession,
        outbound: bool,
    ) -> Option<broadcast::Receiver<Vec<PathUpdate>>> {
        if (self.me < device) != outbound {
            return None;
        }
        let updates = session.updates();
        self.sessions.lock().await.insert(device, Arc::new(session));
        Some(updates)
    }

    /// The devices with an open session.
    pub(crate) async fn devices(&self) -> Vec<GrainId> {
        let sessions = self.sessions.lock().await;
        sessions
            .iter()
            .filter(|(_, session)| session.is_open())
            .map(|(device, _)| *device)
            .collect()
    }

    /// Whether `device` has an open session.
    pub(crate) async fn has(&self, device: &GrainId) -> bool {
        self.sessions
            .lock()
            .await
            .get(device)
            .is_some_and(|session| session.is_open())
    }

    /// Close every session, as if the connections had been cut.
    ///
    /// Dropping a `LiveSession` stops its reader task; the dialer's next walk opens fresh
    /// ones, which is how "a peer that goes away is redialled" holds.
    pub(crate) async fn drop_connections(&self) {
        self.sessions.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dial_backoff_doubles_and_caps_at_five_minutes() {
        assert_eq!(dial_backoff(0), Duration::from_secs(1));
        assert_eq!(dial_backoff(1), Duration::from_secs(2));
        assert_eq!(dial_backoff(2), Duration::from_secs(4));
        assert_eq!(
            dial_backoff(3),
            Duration::from_secs(8),
            "each failure doubles the wait"
        );
        assert_eq!(
            dial_backoff(4),
            Duration::from_secs(16),
            "each failure doubles the wait"
        );
        assert_eq!(
            dial_backoff(9),
            DIAL_BACKOFF_MAX,
            "doubling reaches the cap at nine failures"
        );
        assert_eq!(
            dial_backoff(40),
            DIAL_BACKOFF_MAX,
            "the cap holds however many failures"
        );
    }
}
