//! The data plane: one header line, then bytes.
//!
//! Nothing here looks at what it is copying. The bridge decides *whether* bytes may flow and
//! *where* they go; the replication protocol runs end to end between the app servers at
//! either end.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grain_id::GrainId;
use sapphire_bridge_api::{DataAck, DataHeader, INCOMING, IncomingParams, ManagedBy};
use sapphire_ipc::{Endpoint, RawStream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::Bridge;
use crate::error::{Error, Result};
use crate::invite::Invites;
use crate::net::NetConfig;
use crate::pairing;
use crate::peer::{BoxedStream, Inbound};
use crate::routes::Route;
use crate::wgsync;

/// How long an unclaimed inbound stream is held.
///
/// Long enough for a stopped app server to start (`wake_on_sync`), short enough that a peer
/// that vanishes does not pin a stream for ever.
pub(crate) const TICKET_TTL: Duration = Duration::from_secs(60);

/// An inbound stream waiting for its owner, and when it arrived.
struct Pending {
    stream: BoxedStream,
    created: Instant,
}

/// Inbound streams waiting for their owner to claim them.
///
/// Single use, and expiring: an app server that never comes back must not leave a peer's
/// stream open for the rest of the bridge's life. Pruning happens on every call, so a bridge
/// whose owner never reconnects still releases what it holds.
#[derive(Default)]
pub(crate) struct Tickets {
    pending: Mutex<HashMap<String, Pending>>,
}

impl Tickets {
    /// Park a stream and return its single-use ticket.
    pub(crate) fn park(&self, stream: BoxedStream) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the system random source");
        let ticket = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let mut pending = self.pending.lock().expect("tickets");
        pending.retain(|_, p| !expired(p));
        pending.insert(
            ticket.clone(),
            Pending {
                stream,
                created: Instant::now(),
            },
        );
        ticket
    }

    /// Take a stream. A ticket works once; presenting it again finds nothing.
    pub(crate) fn claim(&self, ticket: &str) -> Option<BoxedStream> {
        let mut pending = self.pending.lock().expect("tickets");
        pending.retain(|_, p| !expired(p));
        pending.remove(ticket).map(|p| p.stream)
    }

    /// Backdate every parked stream, so a test reaches the expiry path without waiting out
    /// [`TICKET_TTL`].
    #[cfg(test)]
    fn age(&self, by: Duration) {
        for pending in self.pending.lock().expect("tickets").values_mut() {
            if let Some(earlier) = pending.created.checked_sub(by) {
                pending.created = earlier;
            }
        }
    }
}

/// Has this stream waited longer than [`TICKET_TTL`]?
fn expired(pending: &Pending) -> bool {
    pending.created.elapsed() >= TICKET_TTL
}

/// Read the header line, without consuming any of the bytes that follow it.
pub(crate) async fn read_header<S>(stream: &mut S) -> Result<DataHeader>
where
    S: tokio::io::AsyncRead + Unpin,
{
    // Capacity 1: a larger buffer would read past the newline and swallow payload bytes. The
    // reader is not kept afterwards, because the bytes after the newline belong to the relay.
    let mut reader = BufReader::with_capacity(1, stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    serde_json::from_str(line.trim())
        .map_err(|e| Error::Protocol(format!("bad data-plane header: {e}")))
}

/// Answer a header, as one JSON line.
pub(crate) async fn write_ack<S>(stream: &mut S, ack: DataAck) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(&ack).map_err(|e| Error::Protocol(e.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    Ok(())
}

/// Copy bytes in both directions until either side closes.
///
/// The bridge never looks at what it is copying: the replication protocol runs end to end
/// between two app servers.
///
/// *Either* side, not both. [`tokio::io::copy_bidirectional`] returns only once both
/// directions have ended, which is right for a proxy whose ends can half-close and wrong
/// here: the far end of a replication session goes quiet while the near end is still
/// waiting for its first frame, so the near-to-far direction never ends on its own. Waiting
/// for it would hold this relay — and with it the app server's connection — open for the
/// rest of the bridge's life, and the app server would wait on a `Hello` from a peer that
/// is already gone. Returning is what drops both halves and lets the app server read end
/// of file; a half-close would not do, because a transport may have none to offer
/// (`poll_shutdown` on a Windows named pipe does nothing).
///
/// Nothing in flight is lost by returning. A direction that ends has copied everything the
/// side that closed had sent, and bytes still queued the other way have nowhere to go: the
/// side they were bound for is the one that closed.
pub(crate) async fn splice<A, B>(a: A, b: B)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    let result = tokio::select! {
        result = tokio::io::copy(&mut a_read, &mut b_write) => result,
        result = tokio::io::copy(&mut b_read, &mut a_write) => result,
    };
    if let Err(err) = result {
        tracing::debug!("a relayed stream ended: {err}");
    }
}

// ── the data listener ───────────────────────────────────────────────────────

/// Serve the data endpoint until it fails.
///
/// Each connection is dispatched on its own task, so one long relay does not delay the
/// streams queued behind it.
pub(crate) async fn listen(bridge: Arc<Bridge>, endpoint: Endpoint) -> Result<()> {
    #[cfg(unix)]
    let listener = sapphire_ipc::bind(&endpoint).await?;
    #[cfg(windows)]
    let mut listener = sapphire_ipc::bind(&endpoint)?;

    loop {
        let stream = listener.accept_raw().await?;
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            if let Err(err) = serve(bridge, stream).await {
                tracing::debug!("a data-plane connection ended: {err}");
            }
        });
    }
}

/// Answer one data-plane connection.
async fn serve(bridge: Arc<Bridge>, mut stream: RawStream) -> Result<()> {
    let header = match read_header(&mut stream).await {
        Ok(header) => header,
        // There is no header to answer: whatever arrived was too mangled to name a reason to.
        Err(err) => {
            tracing::debug!("a data-plane connection sent no usable header: {err}");
            return Err(err);
        }
    };

    match header {
        DataHeader::Open {
            workspace_id,
            device_id,
        } => open(bridge, stream, workspace_id, device_id).await,
        DataHeader::Accept { ticket } => accept(bridge, stream, &ticket).await,
    }
}

/// `Open`: dial the peer that owns `workspace_id`, then relay this connection to it.
async fn open(
    bridge: Arc<Bridge>,
    mut stream: RawStream,
    workspace_id: GrainId,
    device_id: GrainId,
) -> Result<()> {
    match dial(&bridge, workspace_id, device_id).await {
        Ok(peer) => {
            write_ack(
                &mut stream,
                DataAck {
                    ok: true,
                    error: None,
                },
            )
            .await?;
            splice(stream, peer).await;
            Ok(())
        }
        // A refusal is the answer to this connection, not a failure of the bridge: the app
        // server is told why, and the listener keeps serving.
        Err(err) => {
            tracing::debug!("refused an open: {err}");
            refuse(&mut stream, err).await
        }
    }
}

/// Open a stream to the device that owns `workspace_id`, as this host knows it.
async fn dial(bridge: &Bridge, workspace_id: GrainId, device_id: GrainId) -> Result<BoxedStream> {
    // Is this a workspace an app server on this host owns? If not, there is nowhere to go.
    if bridge.route(workspace_id).is_none() {
        return Err(Error::UnknownWorkspace(workspace_id));
    }

    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    let devices = workgroup.devices()?;
    let device = devices
        .get(device_id)
        .ok_or_else(|| Error::Peer(format!("no device {device_id} in this host's workgroup")))?;
    let node_id = device.node_id.clone().ok_or_else(|| {
        Error::Peer(format!(
            "the device {} has not announced a node id yet",
            device.name
        ))
    })?;
    // Whether that device may still be dialed is a live question: a retired device is not a
    // peer, and dialing one would hand this app server a stream that is dropped on arrival.
    workgroup.authorize(&node_id)?;

    bridge.transport().open(&node_id, workspace_id).await
}

/// `Accept`: hand the app server the stream an [`IncomingParams`] announcement named.
async fn accept(bridge: Arc<Bridge>, mut stream: RawStream, ticket: &str) -> Result<()> {
    match bridge.tickets().claim(ticket) {
        Some(peer) => {
            write_ack(
                &mut stream,
                DataAck {
                    ok: true,
                    error: None,
                },
            )
            .await?;
            splice(stream, peer).await;
            Ok(())
        }
        None => {
            refuse(
                &mut stream,
                Error::Protocol("no such ticket, or it has already been used".to_owned()),
            )
            .await
        }
    }
}

/// Answer a data-plane request with a refusal, in the one line its caller is reading.
async fn refuse<S>(stream: &mut S, err: Error) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    write_ack(
        stream,
        DataAck {
            ok: false,
            error: Some(err.to_string()),
        },
    )
    .await
}

/// Start a stopped app server.
///
/// Detached, with no inherited stdio: the bridge is not its parent in any useful sense, and a
/// server that outlives this call is exactly what is wanted.
///
/// Only a route whose `managed_by` is [`ManagedBy::Spawned`] is ever passed here. A
/// service-managed server belongs to the OS service manager, and starting a second copy of
/// one would fight it.
fn wake(route: &Route) -> std::io::Result<()> {
    let mut command = std::process::Command::new(&route.exe_path);
    command
        .args(["server", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and is the documented way to detach. Nothing
        // else happens between fork and exec, which is what `pre_exec` requires.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            })
        };
    }
    let child = command.spawn()?;
    // The server outlives us; do not reap it.
    std::mem::forget(child);
    Ok(())
}

// ── the inbound loop ────────────────────────────────────────────────────────

/// Answer one pairing attempt, as the running bridge promises every holder of a ticket.
///
/// Invites and the workgroup are read fresh — [`Invites::redeem`] re-reads the file, and
/// the ledger is re-opened — because the bridge is long-lived and a joiner must be answered
/// from what is on disk now. An unreadable directory is this host's trouble, which no reply
/// can answer for, so the stream is hung up on and the loop carries on; [`pairing::admit`]
/// itself reports every refusal to the joiner.
async fn answer_pairing(bridge: &Bridge, stream: BoxedStream) {
    let mut invites = match Invites::load(&bridge.dir.root.join("invites.toml")) {
        Ok(invites) => invites,
        Err(err) => {
            tracing::warn!("a pairing attempt arrived, and the invites could not be read: {err}");
            return;
        }
    };
    let workgroup = match bridge.workgroup() {
        Ok(Some(workgroup)) => workgroup,
        Ok(None) => {
            tracing::debug!("a pairing attempt arrived, and this host has no workgroup");
            return;
        }
        Err(err) => {
            tracing::warn!("a pairing attempt arrived, and the workgroup could not be read: {err}");
            return;
        }
    };
    let inviter = match workgroup.this_device(&bridge.transport().node_id()) {
        Ok(device) => device,
        Err(err) => {
            tracing::warn!(
                "a pairing attempt arrived, and this host's own record is unreadable: {err}"
            );
            return;
        }
    };
    if let Err(err) = pairing::admit(stream, &mut invites, &workgroup, &inviter).await {
        tracing::warn!(error = %err, "a pairing attempt was not answered");
    }
}

/// Accept peer streams, authorize them, and park them for the app server that owns them.
///
/// This loop is the only place that sees a peer asking for a workspace whose owner is not
/// running, so `net.wake_on_sync` is decided here: the ask is the cue to start a stopped
/// owner.
pub(crate) async fn inbound(bridge: Arc<Bridge>, net: NetConfig) -> Result<()> {
    loop {
        let (peer_node_id, workspace_id, stream) = match bridge.transport().accept().await? {
            // The pairing gate is the invite secret, not the ledger — a joiner is not a
            // member yet, so [`Workgroup::authorize`](crate::workgroup::Workgroup::authorize) would refuse it by definition. This
            // is the one connection answered without authorization, by the invite flow
            // itself and right here: a running bridge answers any holder of a ticket.
            Inbound::Pairing(_from, stream) => {
                answer_pairing(&bridge, stream).await;
                continue;
            }
            Inbound::Workspace(from, workspace_id, stream) => (from, workspace_id, stream),
            Inbound::Hello(from, stream) => {
                // Authorized exactly like a workspace stream: a stranger learns nothing, and
                // a retired device is not heard.
                match bridge.workgroup()?.map(|wg| wg.authorize(&from)) {
                    Some(Ok(device)) => crate::hello::serve_inbound(&bridge, device.id, stream),
                    _ => {
                        tracing::debug!(peer = %from, "hung up on a hello stream");
                        drop(stream);
                    }
                }
                continue;
            }
        };

        // 1. Authorize before anything else knows a stranger called.
        //
        // Deliberately before step 2: an unauthorized peer must not be able to learn which
        // workspaces this host holds by watching which requests are answered differently.
        // Nothing is ever sent back on a stream that fails this test.
        let Some(workgroup) = bridge.workgroup()? else {
            tracing::debug!(
                peer = %peer_node_id,
                "hung up on an inbound stream: this host has no workgroup"
            );
            drop(stream);
            continue;
        };
        let device = match workgroup.authorize(&peer_node_id) {
            Ok(device) => device,
            Err(err) => {
                tracing::debug!(peer = %peer_node_id, "refused an inbound stream: {err}");
                drop(stream);
                continue;
            }
        };

        // 2. Whose workspace is it?
        //
        // Nobody's, often enough: two hosts enable a workspace at their own pace, so the
        // peer that asks first asks before this host has a route for it. Hanging up is the
        // answer — there is nothing to park the stream for — and the dialler reads the end
        // of its stream and retries. Logged because that retry traffic is otherwise
        // unexplained: a dialler can only report "the peer left before the exchange
        // finished", and this is the line that says why it left.
        let Some(route) = bridge.route(workspace_id) else {
            tracing::debug!(
                peer = %peer_node_id,
                %workspace_id,
                "hung up on an inbound stream: no app server here holds that workspace"
            );
            drop(stream);
            continue;
        };

        // The bridge is the app server of the workgroup's own workspace: no ticket and no
        // announcement, the session is served here.
        //
        // Spawned rather than awaited inline (issue #164): the replica's own `Mutex`
        // already serialises these sessions one at a time, so spawning adds no concurrency
        // there, but awaiting here would hold this loop — and with it every other peer's
        // stream, for any other workspace — behind this one session until it finishes or
        // times out. A peer that parked this stream and has not sent its `Hello` yet
        // blocks for up to `HELLO_TIMEOUT`; nothing else inbound should wait on that.
        if route.app_name == wgsync::WORKSPACE_APP_NAME {
            match bridge.workgroup_replica() {
                Some(replica) => {
                    tokio::spawn(async move {
                        if let Err(err) = replica.session(stream).await {
                            tracing::warn!("a workgroup replication session failed: {err}");
                        }
                    });
                }
                None => tracing::warn!("the workgroup's own workspace has no replica on this host"),
            }
            continue;
        }

        // 3. Park it, so its owner can claim it by ticket, then announce it.
        let ticket = bridge.tickets().park(stream);
        let params = IncomingParams {
            workspace_id,
            peer_device_id: device.id,
            ticket,
        };
        match bridge.owners().peer(&route.app_name) {
            Some(peer) => match serde_json::to_value(params) {
                Ok(params) => {
                    if let Err(err) = peer.notify(INCOMING, params).await {
                        tracing::warn!(app = %route.app_name, "could not announce a peer: {err}");
                    }
                }
                Err(err) => {
                    // Infallible in practice: every field of `IncomingParams` serialises.
                    tracing::warn!(
                        app = %route.app_name,
                        "could not serialise an announcement: {err}"
                    );
                }
            },
            // 4. Nobody home.
            //
            // The ticket is already parked, and its TTL is what gives an owner the bridge
            // starts here the time to start, connect and claim it. A `Service`-managed
            // owner is never started from here: its own service manager starts it, and
            // neither the bridge nor the CLI is that manager. Wake stays `Spawned`-only,
            // so its workspace is reported offline and its service manager is left to
            // answer for it.
            None if net.wake_on_sync && route.managed_by == ManagedBy::Spawned => {
                // `claim` first, and unconditionally: it is what records the attempt, and a
                // failed start must not be retried on every ask either.
                if bridge.wakes().claim(&route.app_name)
                    && let Err(err) = wake(&route)
                {
                    tracing::warn!(app = %route.app_name, "could not start the owner: {err}");
                }
            }
            None => tracing::info!(
                app = %route.app_name,
                managed_by = ?route.managed_by,
                "the owner is not connected; reporting the workspace as offline"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const NODE_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn stream() -> BoxedStream {
        let (mine, _theirs) = tokio::io::duplex(64);
        Box::new(mine)
    }

    #[tokio::test]
    async fn a_ticket_is_consumed_the_first_time_it_is_presented() {
        let tickets = Tickets::default();
        let ticket = tickets.park(stream());
        assert!(tickets.claim(&ticket).is_some());
        assert!(
            tickets.claim(&ticket).is_none(),
            "a single-use ticket that works twice leaves a stream nobody can account for"
        );
    }

    #[tokio::test]
    async fn a_ticket_nobody_claims_expires_rather_than_holding_a_stream_open() {
        let tickets = Tickets::default();
        let ticket = tickets.park(stream());
        tickets.age(TICKET_TTL + Duration::from_secs(1));
        assert!(tickets.claim(&ticket).is_none());

        // A ticket still inside its lifetime is still claimable.
        let fresh = tickets.park(stream());
        tickets.age(Duration::from_millis(1));
        assert!(tickets.claim(&fresh).is_some());
    }

    #[tokio::test]
    async fn parking_a_stream_sweeps_out_the_ones_that_expired() {
        let tickets = Tickets::default();
        let old = tickets.park(stream());
        tickets.age(TICKET_TTL + Duration::from_secs(1));
        let _new = tickets.park(stream());
        assert!(
            tickets.claim(&old).is_none(),
            "parking must drop what has expired"
        );
    }

    #[tokio::test]
    async fn two_parked_streams_get_different_tickets() {
        let tickets = Tickets::default();
        assert_ne!(tickets.park(stream()), tickets.park(stream()));
    }

    #[tokio::test]
    async fn reading_a_header_leaves_the_payload_on_the_stream() {
        let (mut mine, mut theirs) = tokio::io::duplex(256);
        let header = DataHeader::Open {
            workspace_id: GrainId::random(),
            device_id: GrainId::random(),
        };
        let mut line = serde_json::to_vec(&header).unwrap();
        line.push(b'\n');
        line.extend_from_slice(b"raw bytes, not JSON");

        let reader = tokio::spawn(async move {
            let read = read_header(&mut mine).await.unwrap();
            let mut payload = Vec::new();
            mine.read_to_end(&mut payload).await.unwrap();
            (read, payload)
        });

        theirs.write_all(&line).await.unwrap();
        drop(theirs);
        let (read, payload) = reader.await.unwrap();

        assert_eq!(read, header);
        assert_eq!(
            payload, b"raw bytes, not JSON",
            "the relay gets the bytes the header did not consume"
        );
    }

    #[tokio::test]
    async fn a_header_is_read_as_soon_as_its_newline_arrives() {
        // Nothing follows the line, and the peer holds the stream open: the read must return
        // rather than wait for bytes that belong to the payload.
        let (mut mine, mut theirs) = tokio::io::duplex(256);
        let header = DataHeader::Accept {
            ticket: "abc".into(),
        };
        let mut line = serde_json::to_vec(&header).unwrap();
        line.push(b'\n');
        theirs.write_all(&line).await.unwrap();

        let read = tokio::time::timeout(Duration::from_secs(5), read_header(&mut mine))
            .await
            .expect("the header must be read without waiting for more bytes")
            .unwrap();
        assert_eq!(read, header);
    }

    #[tokio::test]
    async fn a_bad_header_is_a_protocol_error_that_names_itself() {
        let (mut mine, mut theirs) = tokio::io::duplex(64);
        theirs
            .write_all(b"{\"kind\":\"sideways\"}\n")
            .await
            .unwrap();
        drop(theirs);
        let err = read_header(&mut mine).await.unwrap_err();
        assert!(err.to_string().contains("header"), "{err}");
    }

    #[tokio::test]
    async fn an_acknowledgement_is_one_json_line() {
        let (mut mine, mut theirs) = tokio::io::duplex(256);
        write_ack(
            &mut mine,
            DataAck {
                ok: true,
                error: None,
            },
        )
        .await
        .unwrap();
        drop(mine);

        let mut line = String::new();
        BufReader::new(&mut theirs)
            .read_line(&mut line)
            .await
            .unwrap();
        let ack: DataAck = serde_json::from_str(line.trim()).unwrap();
        assert!(ack.ok);
    }

    #[tokio::test]
    async fn the_relay_carries_bytes_both_ways_until_one_side_closes() {
        // `app_mine` is the app server's end and `peer_theirs` the peer's; the relay owns the
        // other two halves, exactly as it does in a running bridge.
        let (mut app_mine, app_theirs) = tokio::io::duplex(256);
        let (peer_mine, mut peer_theirs) = tokio::io::duplex(256);
        tokio::spawn(async move {
            splice(app_theirs, peer_mine).await;
        });

        app_mine.write_all(b"to the peer").await.unwrap();
        let mut out = [0u8; 11];
        peer_theirs.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"to the peer");

        peer_theirs.write_all(b"to the app").await.unwrap();
        let mut back = [0u8; 10];
        app_mine.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"to the app");

        drop(peer_theirs);
        let mut end = [0u8; 1];
        assert_eq!(
            app_mine.read(&mut end).await.unwrap(),
            0,
            "closing one side must show as end of file on the other"
        );
    }

    #[tokio::test]
    async fn a_relay_ends_as_soon_as_the_peer_half_closes() {
        // The peer's half is dropped and the app server keeps its own end open, waiting for
        // a `Hello` — which is exactly what the inbound loop leaves behind when it has no
        // route for the workspace a peer asked for.
        //
        // The relay has to *end* there, because ending is what drops the app server's
        // connection and lets it read end of file. A relay that also waited for the
        // app-server-to-peer direction would wait for ever: that direction only ends when
        // the app server closes, and the app server is the one waiting. Half-closing
        // instead of ending is not enough — a transport may have no half-close to offer
        // (`poll_shutdown` on a Windows named pipe does nothing), and then the app server
        // never hears anything at all.
        let (_app_mine, app_theirs) = tokio::io::duplex(256);
        let (peer_mine, peer_theirs) = tokio::io::duplex(256);
        drop(peer_theirs);

        tokio::time::timeout(Duration::from_secs(5), splice(app_theirs, peer_mine))
            .await
            .expect("a relay whose peer has gone must end, not wait on the app server");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_inbound_loop_answers_a_pairing_attempt() {
        // A joiner holds a ticket, not a membership, so its connection must reach the invite
        // flow instead of being hung up on. The bridge is set up exactly as `run` sets it
        // up, except the transport: the loopback here carries the pairing.
        let net = crate::peer::LoopbackNetwork::new();
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::dir::BridgeDir::at(tmp.path().join("bridge")).unwrap();
        let wg = crate::workgroup::Workgroup::create(&dir, "home", "laptop", NODE_A).unwrap();
        let (_invite, secret) = crate::invite::Invites::load(&dir.root.join("invites.toml"))
            .unwrap()
            .create("phone", crate::invite::DEFAULT_TTL)
            .unwrap();

        let bridge = Bridge::new(dir.clone(), Arc::new(net.transport(NODE_A)), "0.0.0").unwrap();
        let loop_task = tokio::spawn(inbound(Arc::new(bridge), NetConfig::default()));

        // Join from a fresh host: the loop above must play the inviter's part.
        let joiner_dir = crate::dir::BridgeDir::at(tmp.path().join("joiner")).unwrap();
        let joined = crate::workgroup::Workgroup::join(
            &joiner_dir,
            &crate::invite::Ticket {
                workgroup_id: wg.id,
                node_addr: NODE_A.as_bytes().to_vec(),
                secret,
                expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            },
            "phone",
            &net.transport(NODE_B),
        )
        .await
        .expect("the pairing attempt must be answered");
        assert_eq!(joined.id, wg.id);
        assert_eq!(
            joined.this_device(NODE_B).unwrap().name,
            "phone",
            "the joiner must have its own record locally"
        );

        loop_task.abort();
        let _ = loop_task.await;
    }
}
