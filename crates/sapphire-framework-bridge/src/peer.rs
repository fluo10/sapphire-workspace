//! How the bridge reaches other devices.
//!
//! An interface, not an implementation: the switchboard, the routing and the authorization
//! are all testable against [`LoopbackTransport`], and iroh is one implementation behind the
//! `node` feature.

use grain_id::GrainId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

// The loopback below is the only user of these, and it is behind the same gate.
#[cfg(any(test, feature = "test-util"))]
use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "test-util"))]
use std::pin::Pin;
#[cfg(any(test, feature = "test-util"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(test, feature = "test-util"))]
use std::sync::{Arc, Mutex};
#[cfg(any(test, feature = "test-util"))]
use std::task::{Context, Poll};
#[cfg(any(test, feature = "test-util"))]
use tokio::io::ReadBuf;
#[cfg(any(test, feature = "test-util"))]
use tokio::sync::mpsc;

#[cfg(any(test, feature = "test-util"))]
use crate::error::Error;
use crate::error::Result;

/// A bidirectional byte stream to another device.
pub trait PeerStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> PeerStream for T {}

/// A boxed [`PeerStream`].
pub type BoxedStream = Box<dyn PeerStream>;

/// What a caller asks a peer for: the first thing sent on a peer stream.
///
/// It travels ahead of the payload so the far side knows which workspace was asked for
/// without looking inside the stream. `node_id` names the caller, which lets a receiver that
/// is told who called by some other means — [`PeerTransport::accept`] reports it — confirm
/// the two agree. Deciding what to do about a disagreement is the bridge's job.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StreamRequest {
    /// The workspace the caller wants.
    pub workspace_id: GrainId,
    /// The caller's own node id.
    pub node_id: String,
}

/// Which protocol an inbound stream arrived to speak.
pub enum Inbound {
    /// A workspace stream: the caller's node id, what it asked for, and the stream. The
    /// ordinary path — authorized with the device ledger before anything is served.
    Workspace(String, GrainId, BoxedStream),
    /// A pairing stream, on its own ALPN: the caller's node id and the stream. The only
    /// connection that arrives **before** membership exists, so it is gated by the invite
    /// secret instead of by the ledger; see [`crate::pairing`].
    Pairing(String, BoxedStream),
    /// A Hello stream, on its own ALPN: the caller's node id and the stream. Authorized with
    /// the ledger like a workspace stream; see [`crate::hello`].
    Hello(String, BoxedStream),
}

/// Reaching other devices.
#[async_trait::async_trait]
pub trait PeerTransport: Send + Sync + 'static {
    /// Open a stream to `node_id` asking for `workspace_id`.
    async fn open(&self, node_id: &str, workspace_id: GrainId) -> Result<BoxedStream>;

    /// Open a stream to `node_addr` speaking the pairing protocol.
    ///
    /// `node_addr` is whatever the ticket's `node_addr` field carries — on iroh, the bytes
    /// of an iroh `NodeAddr`. Taking bytes rather than a typed address is what keeps this
    /// interface free of any one transport's types: a loopback test writes its node id
    /// there and gets a stream to the transport of that name.
    async fn open_pairing(&self, node_addr: &[u8]) -> Result<BoxedStream>;

    /// Open a Hello stream to `node_id`.
    ///
    /// The caller must not open one before its Hello watch holds `Some(Hello)` and must
    /// write at once: on iroh a QUIC stream is invisible to the acceptor until the opener
    /// writes, and the acceptor's accept loop is serial.
    ///
    /// Defaults to an error, so a transport written before the Hello protocol still
    /// compiles. Its bridge then simply hears nobody, and nobody's election counts it.
    async fn open_hello(&self, node_id: &str) -> Result<BoxedStream> {
        Err(crate::error::Error::Peer(format!(
            "this transport cannot open a hello stream to {node_id}"
        )))
    }

    /// Wait for an inbound stream, reporting which protocol it arrived to speak.
    ///
    /// A transport reports who called and what they want; the bridge decides what that is
    /// worth. A pairing stream is answered by [`admit`](crate::pairing::admit), which
    /// skips [`Workgroup::authorize`] — deliberately, and only there: the joiner is not a
    /// member yet, so membership is the one test it cannot pass. Everything else is the
    /// ordinary authorized path.
    async fn accept(&self) -> Result<Inbound>;

    /// Wait for an inbound pairing stream. Returns the caller's node id and the stream.
    ///
    /// The symmetric default is what [`Bridge::run`](crate::Bridge::run) relies on: one transport, one loop,
    /// and every inbound connection — pairing or workspace — answered from it. iroh is
    /// naturally one endpoint and this default is the whole of it there. A workspace-only
    /// caller that still needs pairing to be answered does not exist; use
    /// [`PeerTransport::accept`] and route, as the bridge does.
    async fn accept_pairing(&self) -> Result<(String, BoxedStream)> {
        loop {
            match self.accept().await? {
                Inbound::Pairing(from, stream) => return Ok((from, stream)),
                Inbound::Workspace(..) | Inbound::Hello(..) => continue,
            }
        }
    }

    /// Wait for an inbound workspace stream. Returns the caller's node id, what it asked
    /// for, and the stream.
    ///
    /// The old shape of [`PeerTransport::accept`], for callers that only speak the
    /// workspace protocol. A pairing attempt arriving meanwhile is hung up on, deliberately:
    /// a caller of this shape has said it cannot answer one, and holding the stream open
    /// while never reading it would leave the joiner waiting on a reply that is not coming.
    /// The bridge itself uses [`PeerTransport::accept`] and routes pairing connections to
    /// the invite flow.
    async fn accept_workspace(&self) -> Result<(String, GrainId, BoxedStream)> {
        loop {
            match self.accept().await? {
                Inbound::Workspace(from, workspace_id, stream) => {
                    return Ok((from, workspace_id, stream));
                }
                Inbound::Pairing(_, stream) | Inbound::Hello(_, stream) => drop(stream),
            }
        }
    }

    /// This host's node id.
    fn node_id(&self) -> String;

    /// The bytes to put in a ticket's `node_addr`, so a joiner can dial this host.
    ///
    /// The counterpart of [`PeerTransport::open_pairing`], which reads exactly these bytes
    /// back. Defaults to the node id's own bytes, which is what a transport that is dialed
    /// by name - the loopback - wants. A transport that needs an address as well as an id
    /// overrides this, because only the process holding the bound endpoint knows which
    /// socket addresses are reachable.
    fn ticket_addr(&self) -> Result<Vec<u8>> {
        Ok(self.node_id().into_bytes())
    }

    /// Whether this host can currently reach `node_id`.
    ///
    /// An app server is told which devices of its workgroup are connected
    /// ([`PeerInfo::connected`](sapphire_bridge_api::PeerInfo)), and only the carrier can
    /// answer that: whether a peer is reachable is a property of the network, not of the
    /// ledger.
    ///
    /// Defaults to `false` rather than guessing. A transport that cannot tell must not report
    /// a peer as connected, because the answer is what an app server shows and acts on.
    fn is_connected(&self, node_id: &str) -> bool {
        let _ = node_id;
        false
    }
}

// ── loopback ────────────────────────────────────────────────────────────────

/// Buffer size of each loopback stream, in bytes.
#[cfg(any(test, feature = "test-util"))]
const LOOPBACK_BUFFER: usize = 64 * 1024;

#[cfg(any(test, feature = "test-util"))]
type Inbox = mpsc::UnboundedSender<(String, GrainId, tokio::io::DuplexStream)>;

/// The pairing half of a loopback node's inbox.
#[cfg(any(test, feature = "test-util"))]
type PairingInbox = mpsc::UnboundedSender<(String, tokio::io::DuplexStream)>;

/// A set of transports that can reach each other, with no network.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug, Default)]
pub struct LoopbackNetwork {
    nodes: Arc<Mutex<HashMap<String, Inbox>>>,
    /// The pairing protocol has its own channel per node, as iroh has its own ALPN.
    pairing: Arc<Mutex<HashMap<String, PairingInbox>>>,
    /// Hello streams likewise.
    hello: Arc<Mutex<HashMap<String, PairingInbox>>>,
    /// Writes charged to each node, so a test can watch who is still talking.
    frames: Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>,
    /// When set, only these pairs of node ids may open each other; `None` is a network
    /// where every registered node reaches every other.
    edges: Option<Arc<HashSet<(String, String)>>>,
}

#[cfg(any(test, feature = "test-util"))]
impl LoopbackNetwork {
    /// An empty network.
    pub fn new() -> LoopbackNetwork {
        LoopbackNetwork::default()
    }

    /// A network where only the listed pairs of nodes can open each other.
    ///
    /// Every pair is connected in both directions, and every other pair is unreachable, as
    /// if a firewall sat between them. A node named in no pair can still be registered — a
    /// host every device lists but nobody can reach is exactly what the middle-host and
    /// unreachable-peer tests are about.
    pub fn partitioned(pairs: &[(&str, &str)]) -> LoopbackNetwork {
        let mut edges = HashSet::new();
        for (from, to) in pairs {
            edges.insert(((*from).to_owned(), (*to).to_owned()));
            edges.insert(((*to).to_owned(), (*from).to_owned()));
        }
        LoopbackNetwork {
            nodes: Arc::default(),
            pairing: Arc::default(),
            hello: Arc::default(),
            frames: Arc::default(),
            edges: Some(Arc::new(edges)),
        }
    }

    /// A transport for `node_id`, registered on this network.
    pub fn transport(&self, node_id: &str) -> LoopbackTransport {
        let (tx, rx) = mpsc::unbounded_channel();
        let (pairing_tx, pairing_rx) = mpsc::unbounded_channel();
        let (hello_tx, hello_rx) = mpsc::unbounded_channel();
        let frames = Arc::new(AtomicU64::new(0));
        self.nodes
            .lock()
            .expect("loopback network")
            .insert(node_id.to_owned(), tx);
        self.pairing
            .lock()
            .expect("loopback network")
            .insert(node_id.to_owned(), pairing_tx);
        self.hello
            .lock()
            .expect("loopback network")
            .insert(node_id.to_owned(), hello_tx);
        self.frames
            .lock()
            .expect("loopback network")
            .insert(node_id.to_owned(), Arc::clone(&frames));
        LoopbackTransport {
            node_id: node_id.to_owned(),
            nodes: Arc::clone(&self.nodes),
            inbox: tokio::sync::Mutex::new(rx),
            pairing_nodes: Arc::clone(&self.pairing),
            pairing_inbox: tokio::sync::Mutex::new(pairing_rx),
            hello_nodes: Arc::clone(&self.hello),
            hello_inbox: tokio::sync::Mutex::new(hello_rx),
            frames,
            edges: self.edges.clone(),
        }
    }

    /// How many writes `node_id` has made onto loopback streams so far.
    ///
    /// The loopback counts writes rather than protocol frames: the bridges relay bytes in
    /// buffer-sized chunks, so one frame may arrive as one write or as several. For a test
    /// asking "is this host still sending", the difference does not matter.
    pub fn frames_sent(&self, node_id: &str) -> u64 {
        self.frames
            .lock()
            .expect("loopback network")
            .get(node_id)
            .map_or(0, |count| count.load(Ordering::Relaxed))
    }
}

/// One device's end of a [`LoopbackNetwork`].
#[cfg(any(test, feature = "test-util"))]
#[derive(Debug)]
pub struct LoopbackTransport {
    node_id: String,
    nodes: Arc<Mutex<HashMap<String, Inbox>>>,
    inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, GrainId, tokio::io::DuplexStream)>>,
    pairing_nodes: Arc<Mutex<HashMap<String, PairingInbox>>>,
    pairing_inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>>,
    hello_nodes: Arc<Mutex<HashMap<String, PairingInbox>>>,
    hello_inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>>,
    /// This node's own write counter, charged by every stream it hands out.
    frames: Arc<AtomicU64>,
    /// Who this node may open; `None` is everybody.
    edges: Option<Arc<HashSet<(String, String)>>>,
}

#[cfg(any(test, feature = "test-util"))]
impl LoopbackTransport {
    /// Whether `node_id` may be opened from this node at all.
    fn reachable(&self, node_id: &str) -> bool {
        match &self.edges {
            None => true,
            Some(edges) => edges.contains(&(self.node_id.clone(), node_id.to_owned())),
        }
    }
}

#[cfg(any(test, feature = "test-util"))]
#[async_trait::async_trait]
impl PeerTransport for LoopbackTransport {
    async fn open(&self, node_id: &str, workspace_id: GrainId) -> Result<BoxedStream> {
        if !self.reachable(node_id) {
            // Indistinguishable from "no such node": an unreachable peer is one a test
            // registered to be unreachable, and both mean the dial fails.
            return Err(Error::Peer(format!(
                "no such node on the loopback network: {node_id}"
            )));
        }
        let inbox = {
            let nodes = self.nodes.lock().expect("loopback network");
            nodes.get(node_id).cloned()
        };
        let Some(inbox) = inbox else {
            return Err(Error::Peer(format!(
                "no such node on the loopback network: {node_id}"
            )));
        };
        if inbox.is_closed() {
            return Err(Error::Peer(format!("{node_id} is no longer listening")));
        }
        let (mine, theirs) = tokio::io::duplex(LOOPBACK_BUFFER);
        inbox
            .send((self.node_id.clone(), workspace_id, theirs))
            .map_err(|_| Error::Peer(format!("{node_id} is no longer listening")))?;
        Ok(Box::new(CountingStream {
            inner: mine,
            frames: Arc::clone(&self.frames),
        }))
    }

    async fn open_pairing(&self, node_addr: &[u8]) -> Result<BoxedStream> {
        let node = std::str::from_utf8(node_addr)
            .map_err(|_| Error::Peer("the ticket's address is not a node id".to_owned()))?;
        if !self.reachable(node) {
            return Err(Error::Peer(format!(
                "no such node on the loopback network: {node}"
            )));
        }
        let inbox = {
            self.pairing_nodes
                .lock()
                .expect("loopback network")
                .get(node)
                .cloned()
        };
        let Some(inbox) = inbox else {
            return Err(Error::Peer(format!(
                "no such node on the loopback network: {node}"
            )));
        };
        if inbox.is_closed() {
            return Err(Error::Peer(format!("{node} is no longer listening")));
        }
        let (mine, theirs) = tokio::io::duplex(LOOPBACK_BUFFER);
        inbox
            .send((self.node_id.clone(), theirs))
            .map_err(|_| Error::Peer(format!("{node} is no longer listening")))?;
        Ok(Box::new(CountingStream {
            inner: mine,
            frames: Arc::clone(&self.frames),
        }))
    }

    async fn open_hello(&self, node_id: &str) -> Result<BoxedStream> {
        if !self.reachable(node_id) {
            return Err(Error::Peer(format!(
                "no such node on the loopback network: {node_id}"
            )));
        }
        let inbox = {
            self.hello_nodes
                .lock()
                .expect("loopback network")
                .get(node_id)
                .cloned()
        };
        let Some(inbox) = inbox else {
            return Err(Error::Peer(format!(
                "no such node on the loopback network: {node_id}"
            )));
        };
        let (mine, theirs) = tokio::io::duplex(LOOPBACK_BUFFER);
        inbox
            .send((self.node_id.clone(), theirs))
            .map_err(|_| Error::Peer(format!("{node_id} is no longer listening")))?;
        // Not a `CountingStream`: Hellos are a steady background, and the frame counters
        // exist for tests asking whether *sync* has gone quiet.
        Ok(Box::new(mine))
    }

    async fn accept(&self) -> Result<Inbound> {
        // One `select` over both inboxes is what "the same endpoint with a second ALPN"
        // means here: either kind of caller is answered, whichever dials first.
        tokio::select! {
            item = next_workspace(&self.inbox, Arc::clone(&self.frames)) => item,
            item = next_pairing(&self.pairing_inbox, Arc::clone(&self.frames)) => item,
            item = next_hello(&self.hello_inbox) => item,
        }
    }

    fn node_id(&self) -> String {
        self.node_id.clone()
    }

    fn is_connected(&self, node_id: &str) -> bool {
        // On the loopback, "connected" is exactly "registered and still listening": there is
        // no connection to establish or lose in between. A partitioned network answers for
        // its edges first: an unreachable node is not connected, however alive it is.
        if !self.reachable(node_id) {
            return false;
        }
        self.nodes
            .lock()
            .expect("loopback network")
            .get(node_id)
            .is_some_and(|inbox| !inbox.is_closed())
    }
}

#[cfg(any(test, feature = "test-util"))]
/// The next workspace stream, if the workspace inbox has one.
async fn next_workspace(
    inbox: &tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, GrainId, tokio::io::DuplexStream)>>,
    frames: Arc<AtomicU64>,
) -> Result<Inbound> {
    let mut inbox = inbox.lock().await;
    match inbox.recv().await {
        Some((from, ws, stream)) => Ok(Inbound::Workspace(
            from,
            ws,
            Box::new(CountingStream {
                inner: stream,
                frames,
            }),
        )),
        None => Err(Error::Peer("the loopback network is gone".to_owned())),
    }
}

#[cfg(any(test, feature = "test-util"))]
/// The next Hello stream, from the Hello inbox. Uncounted, like `open_hello`'s end.
async fn next_hello(
    inbox: &tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>>,
) -> Result<Inbound> {
    let mut inbox = inbox.lock().await;
    match inbox.recv().await {
        Some((from, stream)) => Ok(Inbound::Hello(from, Box::new(stream))),
        None => Err(Error::Peer("the loopback network is gone".to_owned())),
    }
}

#[cfg(any(test, feature = "test-util"))]
/// The next pairing attempt, from the pairing inbox.
async fn next_pairing(
    inbox: &tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>>,
    frames: Arc<AtomicU64>,
) -> Result<Inbound> {
    let mut inbox = inbox.lock().await;
    match inbox.recv().await {
        Some((from, stream)) => Ok(Inbound::Pairing(
            from,
            Box::new(CountingStream {
                inner: stream,
                frames,
            }),
        )),
        None => Err(Error::Peer("the loopback network is gone".to_owned())),
    }
}

/// Counts writes onto a stream, as the loopback's stand-in for frames sent.
///
/// The bridges relay session bytes in buffer-sized chunks rather than one write per frame,
/// so the count moves with traffic rather than with the protocol. What a test asks — "is
/// this host still sending?" — the count answers either way.
#[cfg(any(test, feature = "test-util"))]
struct CountingStream<S> {
    inner: S,
    frames: Arc<AtomicU64>,
}

#[cfg(any(test, feature = "test-util"))]
impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

#[cfg(any(test, feature = "test-util"))]
impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) if n > 0 => {
                this.frames.fetch_add(1, Ordering::Relaxed);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// `Result::unwrap_err` wants the success type to be `Debug`, and a peer stream is not:
    /// it is an arbitrary reader and writer. Unwrapping by hand keeps `PeerStream` free of a
    /// `Debug` bound no transport should have to satisfy.
    #[track_caller]
    fn expect_err<T>(result: Result<T>) -> Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }

    #[tokio::test]
    async fn a_loopback_stream_carries_bytes_both_ways() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let ws = GrainId::random();

        let accept = tokio::spawn(async move { b.accept().await });
        let mut opened = a.open("node-b", ws).await.unwrap();

        let accepted = match accept.await.unwrap().unwrap() {
            Inbound::Workspace(from, asked, stream) => (from, asked, stream),
            Inbound::Pairing(..) => panic!("a workspace open arrived as a pairing stream"),
            Inbound::Hello(..) => panic!("a workspace open arrived as a hello stream"),
        };
        let (from, asked, mut accepted) = accepted;
        assert_eq!(from, "node-a");
        assert_eq!(asked, ws);

        opened.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        accepted.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        accepted.write_all(b"world").await.unwrap();
        let mut back = [0u8; 5];
        opened.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"world");
    }

    #[tokio::test]
    async fn opening_to_an_unknown_node_fails() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let err = expect_err(a.open("node-nowhere", GrainId::random()).await);
        assert!(err.to_string().contains("node-nowhere"), "{err}");
    }

    #[tokio::test]
    async fn a_pairing_open_arrives_as_a_pairing_stream() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");

        let accept = tokio::spawn(async move { b.accept().await });
        let mut opened = a.open_pairing(b"node-b").await.unwrap();

        let (from, mut accepted) = match accept.await.unwrap().unwrap() {
            Inbound::Pairing(from, stream) => (from, stream),
            Inbound::Workspace(..) => panic!("a pairing open arrived as a workspace stream"),
            Inbound::Hello(..) => panic!("a pairing open arrived as a hello stream"),
        };
        assert_eq!(from, "node-a");

        opened.write_all(b"pair").await.unwrap();
        let mut buf = [0u8; 4];
        accepted.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pair");
    }

    #[tokio::test]
    async fn a_pairing_attempt_meeting_a_workspace_only_listener_is_hung_up_on() {
        // `accept_workspace` drops pairing streams: a caller of that shape cannot answer
        // them, and leaving the joiner hanging on an unread stream would be worse than a
        // clean close it can retry after.
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let ws = GrainId::random();

        let accept = tokio::spawn(async move { b.accept_workspace().await });
        let mut pairing = a.open_pairing(b"node-b").await.unwrap();
        let _opened = a.open("node-b", ws).await.unwrap();

        let (from, asked, mut accepted) = accept.await.unwrap().unwrap();
        assert_eq!(from, "node-a");
        assert_eq!(asked, ws);

        // The pairing half was dropped on the far side, so the joiner's write fails.
        pairing.write_all(b"pair").await.unwrap_err();
        assert!(matches!(accepted.write_all(b"ok").await, Ok(())));
    }

    #[tokio::test]
    async fn a_transport_reports_its_own_node_id() {
        let net = LoopbackNetwork::new();
        assert_eq!(net.transport("node-a").node_id(), "node-a");
    }

    #[tokio::test]
    async fn closing_one_end_shows_as_end_of_file_on_the_other() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");

        let accept = tokio::spawn(async move { b.accept().await });
        let opened = a.open("node-b", GrainId::random()).await.unwrap();
        let mut accepted = match accept.await.unwrap().unwrap() {
            Inbound::Workspace(_, _, stream) => stream,
            Inbound::Pairing(..) => panic!("a workspace open arrived as a pairing stream"),
            Inbound::Hello(..) => panic!("a workspace open arrived as a hello stream"),
        };

        drop(opened);
        let mut buf = [0u8; 1];
        assert_eq!(accepted.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_registered_node_is_connected_and_an_unknown_one_is_not() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let _b = net.transport("node-b");

        assert!(a.is_connected("node-b"));
        assert!(
            !a.is_connected("node-nowhere"),
            "an unknown node is not connected"
        );
    }

    #[tokio::test]
    async fn a_transport_that_cannot_tell_reports_nothing_connected() {
        // The default is what matters here: an implementation that has no live set must not
        // claim a peer is reachable.
        struct Blind;
        #[async_trait::async_trait]
        impl PeerTransport for Blind {
            async fn open(&self, _: &str, _: GrainId) -> Result<BoxedStream> {
                Err(Error::Peer("no".to_owned()))
            }
            async fn open_pairing(&self, _: &[u8]) -> Result<BoxedStream> {
                Err(Error::Peer("no".to_owned()))
            }
            async fn accept(&self) -> Result<Inbound> {
                Err(Error::Peer("no".to_owned()))
            }
            fn node_id(&self) -> String {
                "blind".to_owned()
            }
        }

        assert!(!Blind.is_connected("anybody"));
    }

    #[tokio::test]
    async fn a_partitioned_network_reaches_only_its_pairs() {
        let net = LoopbackNetwork::partitioned(&[("node-a", "node-s")]);
        let a = net.transport("node-a");
        let s = net.transport("node-s");
        // Alive but unused: keeping the transport open is what has node-b registered and
        // listening for the unreachability assertions below.
        let _b = net.transport("node-b");

        // The listed pair works in both directions. `accept` is called inline so the
        // transport stays alive: the inbox is unbounded, so the open does not have to wait
        // for a listener to be waiting first.
        let opened = a.open("node-s", GrainId::random()).await.unwrap();
        let accepted = match s.accept().await.unwrap() {
            Inbound::Workspace(_, _, stream) => stream,
            Inbound::Pairing(..) => panic!("a workspace open arrived as a pairing stream"),
            Inbound::Hello(..) => panic!("a workspace open arrived as a hello stream"),
        };
        drop((opened, accepted));

        // Everybody else is unreachable, even though the node is registered and listening.
        assert!(
            expect_err(a.open("node-b", GrainId::random()).await)
                .to_string()
                .contains("node-b")
        );
        assert!(
            !a.is_connected("node-b"),
            "an unreachable node is not connected"
        );
        assert!(a.is_connected("node-s"));
    }

    #[tokio::test]
    async fn a_partitioned_pair_is_reachable_in_both_directions() {
        let net = LoopbackNetwork::partitioned(&[("node-a", "node-b")]);
        let a = net.transport("node-a");
        let b = net.transport("node-b");

        let accept = tokio::spawn(async move { b.accept().await });
        let mut opened = a.open("node-b", GrainId::random()).await.unwrap();
        let mut accepted = match accept.await.unwrap().unwrap() {
            Inbound::Workspace(_, _, stream) => stream,
            Inbound::Pairing(..) => panic!("a workspace open arrived as a pairing stream"),
            Inbound::Hello(..) => panic!("a workspace open arrived as a hello stream"),
        };
        opened.write_all(b"there").await.unwrap();
        let mut buf = [0u8; 5];
        accepted.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"there");
    }

    #[tokio::test]
    async fn a_loopback_stream_counts_the_writes_its_owner_made() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        assert_eq!(net.frames_sent("node-a"), 0);

        let accept = tokio::spawn(async move { b.accept().await });
        let mut opened = a.open("node-b", GrainId::random()).await.unwrap();
        let mut accepted = match accept.await.unwrap().unwrap() {
            Inbound::Workspace(_, _, stream) => stream,
            Inbound::Pairing(..) => panic!("a workspace open arrived as a pairing stream"),
            Inbound::Hello(..) => panic!("a workspace open arrived as a hello stream"),
        };

        opened.write_all(b"hello").await.unwrap();
        accepted.read_exact(&mut [0u8; 5]).await.unwrap();
        assert!(
            net.frames_sent("node-a") >= 1,
            "a write onto the stream is charged to the stream's owner"
        );
        assert_eq!(
            net.frames_sent("node-b"),
            0,
            "the receiving side has written nothing yet"
        );
    }
}
