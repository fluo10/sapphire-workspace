//! One replication session over one stream.
//!
//! Both sides run this same function. It is symmetric: each sends what the other lacks and
//! applies what it is sent, and neither is a client.
//!
//! The exchange, in order:
//!
//! 1. `Hello` — format, workspace, replica id and the sender's version vector. A format
//!    mismatch, or a different workspace, ends the session with `Refused` before any state
//!    is exchanged.
//! 2. Every path state the peer's version vector does not cover, in pages of [`PAGE`], each
//!    page preceded by the content it inlines (64 KiB or less), then `Done`.
//! 3. `Want` for content that was not inlined, answered with a blob or `Missing`.
//! 4. `Settled`, once the sender will ask for nothing more.
//! 5. `commit_session` last. An interrupted session leaves the version vector where it was,
//!    and everything is simply resent next time; committing earlier would make the replica
//!    claim versions whose content never arrived.
//!
//! The exchange is one function, [`initial_exchange`], because two callers start here:
//! `run_session` finishes a session from what it returns, and `open_live_session` (in
//! `live.rs`) keeps the stream and applies `Live` pushes on it.
//!
//! # Why there are two end markers
//!
//! `Done` says "my pages are complete" and is sent as soon as they are queued, so the peer
//! can start asking for content without waiting for anything. That leaves a race worth
//! naming: a peer that has just applied a page and is about to send `Want` looks exactly
//! like a peer that needs nothing. A side that stopped reading at `Done` would therefore
//! drop the `Want` for every file over [`INLINE_LIMIT`] — and, with it, the file. `Settled`
//! closes the race. It is sent once the sender's want list can no longer grow (the peer's
//! pages are in and every hash asked for has been answered), so the receiver knows no `Want`
//! can follow, and both sides exit only after seeing the peer's `Done` *and* `Settled`.
//!
//! Content is addressed by its hash, so a page's inline blobs may travel in front of the page
//! that references them. They do: a receiver that had already applied the page would
//! otherwise ask for content that is on the wire, turning every inlined file into a `Want`
//! and a second copy of its bytes.
//!
//! # Writers
//!
//! Everything this side sends goes through one task, so the read loop never blocks on a
//! write. Two `run_session` calls facing each other would otherwise deadlock the moment both
//! wrote more than the socket buffer while each waited to read.

use std::collections::{HashMap, HashSet};

use grain_id::GrainId;
use sapphire_sync::{
    Content, ContentHash, ContentSource, PathUpdate, Replica, Report, VersionVector,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::frame::{Frame, read_frame, write_blob, write_control};
use crate::message::Message;
use crate::{INLINE_LIMIT, SESSION_FORMAT_VERSION};

/// How many path states travel in one `Updates` message.
const PAGE: usize = 256;

/// How many frames may wait for the writer before the read loop waits with them. Bounds how
/// far one peer can make the other buffer by asking faster than it reads.
const QUEUE: usize = 64;

/// How long the exchange waits for the peer's `Hello`.
///
/// The one wait in a session with nothing behind it. Every later read is answering something
/// the peer asked for or was told, and a peer that stops mid-exchange ends the read with end
/// of file. The `Hello` is different: the stream is open, the peer has its end, and it may
/// simply never speak — a bridge that parked the stream for an app server which never
/// claimed it, or a peer that went away between the dial and its first frame on a transport
/// with no half-close to report it with.
///
/// Waiting for ever is not an option, because the caller holds the replica's lock across the
/// exchange: one silent peer would pin this host's replica for the rest of the process's
/// life, and no scan, no commit and no other session would get through. The same reasoning
/// the bridge's workgroup driver applies to a whole session.
///
/// Generous on purpose, and a bound rather than a pace: a peer's bridge may hold a stream
/// parked for about a minute while it starts a stopped app server, which is waiting that is
/// working as intended, and nothing waits this out when the peer answers.
pub const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// What a session did.
#[derive(Clone, Debug, Default)]
pub struct SessionOutcome {
    /// Path updates sent.
    pub sent: usize,
    /// Path updates received.
    pub received: usize,
    /// What applying them did.
    pub report: Report,
}

/// One thing to put on the wire.
pub(crate) enum Out {
    Control(Message),
    Blob(ContentHash, Vec<u8>),
}

/// Content received inline or fetched during this session.
#[derive(Default)]
pub(crate) struct Received(pub(crate) HashMap<ContentHash, Vec<u8>>);

impl ContentSource for Received {
    fn fetch(&self, hash: &ContentHash) -> Option<Vec<u8>> {
        self.0.get(hash).cloned()
    }
}

/// Why the read loop stopped.
pub(crate) enum Stop {
    /// The peer's `Done` and `Settled` are in and this side wants nothing more.
    Complete,
    /// The stream ended; the peer is gone.
    Gone,
    /// The peer refused to continue, and why.
    Refused(String),
    /// The peer sent something this session does not allow.
    Protocol(String),
}

/// Everything the initial exchange left behind, for the caller that decides what happens
/// next.
pub(crate) struct Exchange<S> {
    /// The peer's half of the stream, still to be read.
    pub(crate) reader: ReadHalf<S>,
    /// Everything this side sends goes through here, and one task drains it in order.
    pub(crate) out: mpsc::Sender<Out>,
    /// The task owning the write half.
    pub(crate) writer: JoinHandle<()>,
    /// The task queueing this side's pages and its `Done`.
    pub(crate) queued: JoinHandle<()>,
    /// Everything the peer said it has, as of its `Hello`.
    pub(crate) peer_vv: VersionVector,
    /// Content received inline, or fetched, during the exchange.
    pub(crate) received: Received,
    /// What the exchange did.
    pub(crate) outcome: SessionOutcome,
    /// How the read loop stopped.
    pub(crate) stop: Stop,
}

/// Run a session to completion.
///
/// Returns once both sides have said `Done` and `Settled` and every piece of content this
/// side asked for has been answered — with the bytes, or with `Missing`. A stream that ends
/// first returns without an error, but commits nothing.
///
/// `replica` is locked only from after the peer's `Hello` is in through to the end of the
/// exchange — not while waiting for it (issue #163). A caller that used to pre-lock a
/// `Mutex<Replica>` around the whole call should stop: passing the mutex itself is what lets
/// a peer merely taking its time with `Hello` (up to [`HELLO_TIMEOUT`]) leave the replica
/// free for a scan or another session meanwhile.
///
/// `S` must be `'static` because the write half is moved into a task; a stream owned by the
/// caller (a socket, a spliced pipe, a duplex) is.
pub async fn run_session<S>(
    stream: S,
    replica: &Mutex<Replica>,
    workspace_id: GrainId,
) -> Result<SessionOutcome>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let Exchange {
        out,
        writer,
        queued,
        peer_vv,
        received,
        outcome,
        stop,
        ..
    } = initial_exchange(stream, replica, workspace_id).await?;
    match stop {
        Stop::Complete => {
            // The peer is still reading: it exits only once it has seen this side's
            // `Settled`, which is queued but not yet written. Draining the writer delivers
            // `Done`, `Settled` and every pending answer.
            let _ = queued.await;
            let result = {
                let mut locked = replica.lock().await;
                materialise(&mut locked, &received, &peer_vv, outcome)
            };
            close(out, writer, result).await
        }
        // The peer is gone or has misbehaved, so there is nothing to deliver and a write
        // may never complete. Abort: nothing is committed either way, and everything is
        // resent next session.
        Stop::Gone => {
            queued.abort();
            abort(out, writer);
            Ok(outcome)
        }
        Stop::Refused(why) => {
            queued.abort();
            abort(out, writer);
            Err(Error::Refused(why))
        }
        Stop::Protocol(why) => {
            queued.abort();
            abort(out, writer);
            Err(Error::Protocol(why))
        }
    }
}

/// Run the initial exchange: `Hello`, this side's pages, and reading until the peer's
/// `Done` and `Settled` are in and every hash this side asked for has been answered.
///
/// Both session kinds start here. Nothing is committed: the caller decides, so that an
/// interrupted exchange leaves the version vector where it was. An error means the exchange
/// never reached its end markers, and `run_session`'s own error mapping is the caller's job.
///
/// `replica` is locked twice, not once for the whole call (issue #163): briefly at the very
/// start, to read the two values this side's own `Hello` needs, and again once the peer's
/// `Hello` is in, held from there through the rest of the exchange — which does need it
/// continuously, applying and committing each page as it arrives. The gap between the two is
/// the point: nothing from here to the peer's `Hello` touches the replica, so a peer that
/// merely takes its time (up to [`HELLO_TIMEOUT`]) must not pin it while this side waits.
pub(crate) async fn initial_exchange<S>(
    stream: S,
    replica: &Mutex<Replica>,
    workspace_id: GrainId,
) -> Result<Exchange<S>>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    // The writer task owns the write half and drains the channel in order, so the read loop
    // is never blocked by a peer that is slow to read.
    let (out_tx, mut out_rx) = mpsc::channel::<Out>(QUEUE);
    let writer_task = tokio::spawn(async move {
        while let Some(item) = out_rx.recv().await {
            let result = match item {
                Out::Control(msg) => write_control(&mut writer, &msg).await,
                Out::Blob(hash, bytes) => write_blob(&mut writer, &hash, &bytes).await,
            };
            if result.is_err() {
                break;
            }
        }
    });

    // 1. Hello, before anything else. Reading `replica_id`/`vv` is instantaneous, so the
    // lock is released again immediately — nothing below here needs the replica until the
    // peer's own `Hello` is in.
    let (replica_id, vv) = {
        let locked = replica.lock().await;
        (locked.replica_id(), locked.vv().clone())
    };
    send(
        &out_tx,
        Out::Control(Message::Hello {
            format: SESSION_FORMAT_VERSION,
            workspace_id,
            replica_id,
            vv,
        }),
    )
    .await?;

    // A peer that never speaks is given up on rather than waited out: see [`HELLO_TIMEOUT`].
    // The queue is dropped rather than drained, because a peer that did not send a `Hello`
    // cannot be expected to be reading either.
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader)).await {
        Ok(frame) => frame,
        Err(_) => {
            abort(out_tx, writer_task);
            return Err(Error::Protocol(format!(
                "the peer sent no Hello within {HELLO_TIMEOUT:?}"
            )));
        }
    };

    let peer_vv = match hello? {
        Some(Frame::Control(Message::Hello {
            format,
            workspace_id: theirs,
            vv,
            ..
        })) => {
            if format != SESSION_FORMAT_VERSION {
                let _ = send(
                    &out_tx,
                    Out::Control(Message::Refused(format!(
                        "session format {format}, expected {SESSION_FORMAT_VERSION}"
                    ))),
                )
                .await;
                return refuse(
                    out_tx,
                    writer_task,
                    Error::VersionMismatch {
                        ours: SESSION_FORMAT_VERSION,
                        theirs: format,
                    },
                )
                .await;
            }
            if theirs != workspace_id {
                let _ = send(
                    &out_tx,
                    Out::Control(Message::Refused(format!(
                        "this stream is for workspace {workspace_id}, not {theirs}"
                    ))),
                )
                .await;
                return refuse(
                    out_tx,
                    writer_task,
                    Error::Protocol(format!(
                        "workspace mismatch: the peer is syncing {theirs}, this session is for \
                     {workspace_id}"
                    )),
                )
                .await;
            }
            vv
        }
        Some(Frame::Control(Message::Refused(why))) => {
            return refuse(out_tx, writer_task, Error::Refused(why)).await;
        }
        other => {
            return refuse(
                out_tx,
                writer_task,
                Error::Protocol(format!("expected Hello, got {other:?}")),
            )
            .await;
        }
    };

    // The peer's `Hello` is in: from here to the end of the exchange, every step reads or
    // writes the replica, so the lock is held continuously rather than re-taken per step.
    let mut replica = replica.lock().await;

    // 2. Build the outgoing side as owned data, then hand it to a task. Computing it up
    //    front keeps the read loop free while it is written.
    let delta = replica
        .delta_for(&peer_vv)
        .map_err(|e| Error::Sync(e.to_string()))?;
    let sent = delta.len();
    let mut outgoing: Vec<Out> = Vec::new();
    for page in delta.chunks(PAGE) {
        for update in page {
            for entry in &update.versions {
                let Content::File { hash, len } = entry.content else {
                    continue;
                };
                if len as usize > INLINE_LIMIT {
                    continue;
                }
                if let Ok(Some(bytes)) = replica.read_content(&hash) {
                    outgoing.push(Out::Blob(hash, bytes));
                }
            }
        }
        outgoing.push(Out::Control(Message::Updates(page.to_vec())));
    }
    outgoing.push(Out::Control(Message::Done));

    let sender = out_tx.clone();
    let send_task = tokio::spawn(async move {
        for item in outgoing {
            if sender.send(item).await.is_err() {
                break;
            }
        }
    });

    // 3. Read until the peer is done and settled and every hash we asked for has been
    //    answered.
    let mut received = Received::default();
    let mut wanted: HashSet<ContentHash> = HashSet::new();
    let mut report = Report::default();
    let mut received_count = 0usize;
    let mut peer_done = false;
    let mut peer_settled = false;
    let mut settled = false;

    let stop = loop {
        if peer_done && peer_settled && wanted.is_empty() {
            break Stop::Complete;
        }
        let Some(frame) = read_frame(&mut reader).await? else {
            break Stop::Gone;
        };
        match frame {
            Frame::Control(Message::Updates(updates)) => {
                received_count += updates.len();
                // The join is idempotent, so a redelivered page costs nothing.
                let page = replica
                    .apply(&updates, &received)
                    .map_err(|e| Error::Sync(e.to_string()))?;
                merge_report(&mut report, page);
                // Ask for content the page needs that this session does not have.
                for hash in needed(&replica, &updates, &received)? {
                    if wanted.insert(hash) {
                        send(&out_tx, Out::Control(Message::Want(hash))).await?;
                    }
                }
            }
            Frame::Control(Message::Done) => peer_done = true,
            Frame::Control(Message::Settled) => peer_settled = true,
            Frame::Control(Message::Want(hash)) => {
                let answer = match replica.read_content(&hash) {
                    Ok(Some(bytes)) => Out::Blob(hash, bytes),
                    _ => Out::Control(Message::Missing(hash)),
                };
                send(&out_tx, answer).await?;
            }
            Frame::Control(Message::Missing(hash)) => {
                // The peer does not have it either. Another peer may; the path simply stays
                // unmaterialised until one does.
                wanted.remove(&hash);
            }
            Frame::Control(Message::Live(_)) => {
                // A `Live` push travels only after both sides have left the exchange, and
                // frames from one peer arrive in order, so a compliant peer cannot send one
                // here.
                break Stop::Protocol(
                    "a Live push before the initial exchange finished".to_owned(),
                );
            }
            Frame::Control(Message::Refused(why)) => {
                break Stop::Refused(why);
            }
            Frame::Control(Message::Hello { .. }) => {
                break Stop::Protocol("a second Hello".to_owned());
            }
            Frame::Blob { hash, bytes } => {
                // Verify before storing: content is addressed by hash, so a peer that sent
                // the wrong bytes under one must not be able to plant them.
                if ContentHash::of_bytes(&bytes) != hash {
                    break Stop::Protocol(format!("content does not match {hash}"));
                }
                wanted.remove(&hash);
                received.0.insert(hash, bytes);
            }
        }
        // The peer's pages are all in, so this side's want list can no longer grow: settling
        // tells the peer it may close as soon as it wants nothing more either.
        if peer_done && wanted.is_empty() && !settled {
            settled = true;
            send(&out_tx, Out::Control(Message::Settled)).await?;
        }
    };

    Ok(Exchange {
        reader,
        out: out_tx,
        writer: writer_task,
        queued: send_task,
        peer_vv,
        received,
        outcome: SessionOutcome {
            sent,
            received: received_count,
            report,
        },
        stop,
    })
}

/// Materialise what was waiting on content, then commit.
///
/// Committing is what makes this replica claim it has what the peer had. It runs last, and
/// only when the peer said `Done` and `Settled`; an interrupted session leaves the version
/// vector where it was.
pub(crate) fn materialise(
    replica: &mut Replica,
    received: &Received,
    peer_vv: &VersionVector,
    mut outcome: SessionOutcome,
) -> Result<SessionOutcome> {
    let fetched = replica
        .fetch_missing(received)
        .map_err(|e| Error::Sync(e.to_string()))?;
    merge_report(&mut outcome.report, fetched);
    replica
        .commit_session(peer_vv)
        .map_err(|e| Error::Sync(e.to_string()))?;
    tracing::debug!(
        sent = outcome.sent,
        received = outcome.received,
        changed = outcome.report.changed,
        "session complete"
    );
    Ok(outcome)
}

/// Queue one frame, or report that the writer is gone.
pub(crate) async fn send(tx: &mpsc::Sender<Out>, item: Out) -> Result<()> {
    tx.send(item)
        .await
        .map_err(|_| Error::Protocol("the stream closed".to_owned()))
}

/// Close the outgoing side, let the writer flush what is queued, and return `result`.
///
/// Dropping the sender is what ends the writer task; it drains first, so a `Refused` or a
/// `Settled` queued moments ago still reaches the peer before the stream goes away.
async fn close(
    out_tx: mpsc::Sender<Out>,
    writer: JoinHandle<()>,
    result: Result<SessionOutcome>,
) -> Result<SessionOutcome> {
    drop(out_tx);
    let _ = writer.await;
    result
}

/// Send a `Refused` that is already queued, close the writer, and return `err`.
///
/// The exchange's own early exits: the peer is still reading, so what was queued has to
/// reach it before the stream goes away.
async fn refuse<S>(
    out_tx: mpsc::Sender<Out>,
    writer: JoinHandle<()>,
    err: Error,
) -> Result<Exchange<S>> {
    drop(out_tx);
    let _ = writer.await;
    Err(err)
}

/// Close the outgoing side without waiting to deliver what is queued.
///
/// Used only once the peer can no longer be expected to read, where a write may otherwise
/// never complete.
pub(crate) fn abort(out_tx: mpsc::Sender<Out>, writer: JoinHandle<()>) {
    drop(out_tx);
    writer.abort();
}

/// Hashes these updates need that neither the store nor this session has.
pub(crate) fn needed(
    replica: &Replica,
    updates: &[PathUpdate],
    received: &Received,
) -> Result<Vec<ContentHash>> {
    let mut out = Vec::new();
    for update in updates {
        for entry in &update.versions {
            let Some(hash) = entry.content.hash() else {
                continue;
            };
            if received.0.contains_key(&hash) {
                continue;
            }
            let have = replica
                .read_content(&hash)
                .map_err(|e| Error::Sync(e.to_string()))?
                .is_some();
            if !have {
                out.push(hash);
            }
        }
    }
    Ok(out)
}

pub(crate) fn merge_report(into: &mut Report, from: Report) {
    into.changed += from.changed;
    into.recorded.extend(from.recorded);
    into.conflicts.extend(from.conflicts);
    into.skipped.extend(from.skipped);
}
