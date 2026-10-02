//! The phase after `Done`: a session that stays open and pushes what is committed.
//!
//! [`open_live_session`] runs the same initial exchange as [`crate::run_session`] and then,
//! instead of closing the stream, keeps reading it. `Live` messages are applied as they
//! arrive and published on a broadcast, and [`LiveSession::push`] sends a batch through the
//! same writer task the exchange used. The two ends of one session are two independent
//! `LiveSession`s: each side pushes what it commits and applies what arrives, and neither is
//! a client.
//!
//! Content does not travel inside a `Live` message. A batch that names a file this side does
//! not have is answered with `Want` on the same stream, and the peer's `Blob` settles it
//! through the machinery the exchange already uses. A batch is published only once every
//! hash it was waiting on has been answered, so a host that forwards what it receives never
//! announces an entry it cannot serve.
//!
//! # A dead peer must not stall the others
//!
//! The write half belongs to one task fed by a bounded channel, so a peer that stopped
//! reading only fills its own queue. [`LiveSession::push`] fails at once once the session has
//! closed, so a caller looping over peers moves on to the next one instead of waiting on one
//! that went away.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use grain_id::GrainId;
use sapphire_sync::{ContentHash, PathUpdate, Replica, VersionVector};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::frame::{Frame, read_frame};
use crate::message::Message;
use crate::session::{self, Exchange, Out, SessionOutcome, Stop, send};

/// How many batches may wait for a subscriber before the oldest is dropped. Applies to the
/// broadcast of received updates, not to the wire.
const LIVE_QUEUE: usize = 64;

/// A session that stays open after the initial exchange.
///
/// Dropping it closes the session; `close` is the same thing said out loud.
pub struct LiveSession {
    out: mpsc::Sender<Out>,
    updates: broadcast::Sender<Vec<PathUpdate>>,
    peer_vv: Arc<Mutex<VersionVector>>,
    open: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl LiveSession {
    /// Send updates to the peer.
    ///
    /// Fails rather than waiting once the session has closed, so a caller looping over
    /// peers does not stall on one that went away.
    pub async fn push(&self, updates: Vec<PathUpdate>) -> Result<()> {
        if !self.is_open() {
            return Err(Error::Protocol("the session is closed".to_owned()));
        }
        self.out
            .send(Out::Control(Message::Live(updates)))
            .await
            .map_err(|_| Error::Protocol("the session is closed".to_owned()))
    }

    /// Updates that arrived from the peer, after they were applied.
    ///
    /// A batch is published once every hash it was waiting on has been answered — with the
    /// bytes, or with `Missing` — so a subscriber forwarding it forwards what this host can
    /// answer for.
    pub fn updates(&self) -> broadcast::Receiver<Vec<PathUpdate>> {
        self.updates.subscribe()
    }

    /// Everything the peer is known to have.
    ///
    /// Consulted before forwarding: an entry the peer already covers must not be sent again,
    /// or three connected hosts will pass one edit round in a circle. It advances as the
    /// peer's batches arrive, because each one carries what the peer had when it sent it.
    pub fn peer_vv(&self) -> VersionVector {
        self.peer_vv.lock().expect("peer vv").clone()
    }

    /// Is the session still usable?
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// Close it.
    pub fn close(self) {
        drop(self);
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.open.store(false, Ordering::Relaxed);
        self.reader.abort();
    }
}

/// A batch that arrived and is waiting for content before it can be announced.
struct Pending {
    /// What the peer sent.
    batch: Vec<PathUpdate>,
    /// The hashes this batch is still waiting on. Empty means it is ready to publish.
    outstanding: HashSet<ContentHash>,
}

/// Run the initial exchange and then keep the session open.
///
/// Both sides call this: each sends what the other lacks during the exchange and afterwards
/// pushes what it commits, so the returned [`LiveSession`] is the same handle on either end
/// of the stream. The [`SessionOutcome`] is returned as soon as both sides have said `Done`
/// and `Settled` — "we are caught up" — while the session itself keeps going until a side
/// closes it or the stream ends.
///
/// The replica is locked once the peer's `Hello` is in — not while waiting for it (issue
/// #163) — and held from there through the exchange and for every arriving batch, so a scan
/// or a local commit on this side waits behind one batch rather than behind the session.
/// Nothing is committed when the peer goes away or breaks the protocol: the error says which
/// happened, and the replica's version vector is left where it was.
///
/// `S` must be `'static` because both halves of the stream are moved into tasks; a stream
/// owned by the caller (a socket, a spliced pipe, a duplex) is.
pub async fn open_live_session<S>(
    stream: S,
    replica: Arc<tokio::sync::Mutex<Replica>>,
    workspace_id: GrainId,
) -> Result<(SessionOutcome, LiveSession)>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let exchange = session::initial_exchange(stream, &replica, workspace_id).await?;

    // A session is only live once the exchange reached its end markers. Anything else is
    // reported as the failure it is: `run_session` may treat a peer that simply left as
    // "nothing happened", but a caller asking for a live session cannot.
    if !matches!(&exchange.stop, Stop::Complete) {
        let why = match &exchange.stop {
            Stop::Gone => Error::Protocol("the peer left before the exchange finished".to_owned()),
            Stop::Refused(why) => Error::Refused(why.clone()),
            Stop::Protocol(why) => Error::Protocol(why.clone()),
            Stop::Complete => unreachable!("checked above"),
        };
        exchange.queued.abort();
        session::abort(exchange.out, exchange.writer);
        return Err(why);
    }

    let Exchange {
        reader,
        out,
        writer: _,
        queued,
        peer_vv,
        mut received,
        outcome,
        stop: _,
    } = exchange;

    // The peer is still reading, so what this side queued during the exchange — its last
    // pages, its `Done` — has to reach the wire before the live phase starts writing.
    let _ = queued.await;

    // Caught up: materialise what was waiting on content and commit, exactly as
    // `run_session` does at this point.
    let outcome = {
        let mut locked = replica.lock().await;
        session::materialise(&mut locked, &received, &peer_vv, outcome)?
    };

    let peer_vv = Arc::new(Mutex::new(peer_vv));
    let open = Arc::new(AtomicBool::new(true));
    let (updates, _) = broadcast::channel(LIVE_QUEUE);

    let reader_task = {
        let replica = Arc::clone(&replica);
        let peer_vv = Arc::clone(&peer_vv);
        let open = Arc::clone(&open);
        let updates = updates.clone();
        let out = out.clone();
        let mut reader = reader;
        // Hashes asked for and not yet answered. One `Want` for a hash however many batches
        // need it.
        let mut wanted: HashSet<ContentHash> = HashSet::new();
        // Batches joined but not yet announced; each is held until its own hashes are in.
        let mut pending: Vec<Pending> = Vec::new();
        tokio::spawn(async move {
            loop {
                // Set when content landed, so the files behind it are written before the
                // batches waiting on it are announced.
                let mut wrote_content = false;
                match read_frame(&mut reader).await {
                    Ok(Some(Frame::Control(Message::Live(batch)))) => {
                        // The peer had all of this when it sent the batch, so its own
                        // version vector covers it. Merging now is what keeps a forwarder
                        // from sending the same entries back for ever.
                        {
                            let mut known = peer_vv.lock().expect("peer vv");
                            for update in &batch {
                                known.merge(&update.seen);
                            }
                        }
                        let missing = {
                            let mut locked = replica.lock().await;
                            // The join is idempotent, so a repeated batch costs nothing.
                            if let Err(err) = locked.apply(&batch, &received) {
                                tracing::warn!("a live batch was not applied: {err}");
                                break;
                            }
                            match session::needed(&locked, &batch, &received) {
                                Ok(missing) => missing,
                                Err(err) => {
                                    tracing::warn!("a live batch could not be checked: {err}");
                                    break;
                                }
                            }
                        };
                        // Content travels by hash, so a batch naming a file this side does
                        // not have is completed by asking for it on this same stream.
                        let mut outstanding = HashSet::new();
                        for hash in missing {
                            if wanted.insert(hash)
                                && send(&out, Out::Control(Message::Want(hash))).await.is_err()
                            {
                                return;
                            }
                            outstanding.insert(hash);
                        }
                        pending.push(Pending { batch, outstanding });
                    }
                    Ok(Some(Frame::Control(Message::Want(hash)))) => {
                        let answer = {
                            let locked = replica.lock().await;
                            match locked.read_content(&hash) {
                                Ok(Some(bytes)) => Out::Blob(hash, bytes),
                                _ => Out::Control(Message::Missing(hash)),
                            }
                        };
                        if send(&out, answer).await.is_err() {
                            break;
                        }
                    }
                    Ok(Some(Frame::Blob { hash, bytes })) => {
                        // Verify before storing: content is addressed by hash, so a peer
                        // that sent the wrong bytes under one must not be able to plant
                        // them.
                        if ContentHash::of_bytes(&bytes) != hash {
                            tracing::warn!("content does not match {hash}");
                            break;
                        }
                        remember(&mut wanted, &mut pending, hash);
                        received.0.insert(hash, bytes);
                        wrote_content = true;
                    }
                    // The peer does not have it either. Another peer may; the path simply
                    // stays unmaterialised until one does, and the batches that wanted it
                    // can be announced without it.
                    Ok(Some(Frame::Control(Message::Missing(hash)))) => {
                        remember(&mut wanted, &mut pending, hash);
                    }
                    // The exchange is over and frames arrive in order, so `Done`, `Settled`,
                    // `Updates`, a second `Hello` or a `Refused` cannot follow here.
                    Ok(Some(other)) => {
                        tracing::warn!("a live session got an unexpected frame: {other:?}");
                        break;
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!("a live session stopped reading: {err}");
                        break;
                    }
                }

                if wrote_content {
                    // Write the bytes to disk before any batch that names them is
                    // announced: a subscriber reads the file, not the store.
                    let mut locked = replica.lock().await;
                    if let Err(err) = locked.fetch_missing(&received) {
                        tracing::warn!("fetched content did not settle: {err}");
                        break;
                    }
                }
                // Announce every batch that is no longer waiting on anything. Failing only
                // means nobody is listening.
                pending.retain(|entry| {
                    if entry.outstanding.is_empty() {
                        let _ = updates.send(entry.batch.clone());
                        false
                    } else {
                        true
                    }
                });
            }
            open.store(false, Ordering::Relaxed);
        })
    };

    let live = LiveSession {
        out,
        updates,
        peer_vv,
        open,
        reader: reader_task,
    };
    Ok((outcome, live))
}

/// Record that `hash` is answered: it is no longer wanted, and no pending batch waits on it.
fn remember(wanted: &mut HashSet<ContentHash>, pending: &mut [Pending], hash: ContentHash) {
    wanted.remove(&hash);
    for entry in pending.iter_mut() {
        entry.outstanding.remove(&hash);
    }
}
