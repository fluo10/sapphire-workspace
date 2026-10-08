//! The iroh implementation of [`PeerTransport`](crate::PeerTransport).
//!
//! Everything iroh-shaped lives here. The rest of the bridge knows only "open a stream to a
//! node id" and "accept a stream and learn who called", so replacing this file would not
//! touch anything else.

use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use grain_id::GrainId;
use sapphire_bridge_api::ALPN;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};

// `iroh` is also the name of this module, so the crate is spelled with a leading `::` or the
// path would be ambiguous.
use std::sync::Arc;

use ::iroh::address_lookup::{DnsAddressLookup, MemoryLookup, PkarrPublisher, PkarrResolver};
use ::iroh::endpoint::presets;
use ::iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr,
    defaults::prod::default_relay_map,
};

use crate::error::{Error, Result};
use crate::hello::HELLO_ALPN;
use crate::net::NetConfig;
use crate::pairing::PAIR_ALPN;
use crate::peer::{BoxedStream, Inbound, PeerTransport, StreamRequest};
use crate::relay::RelayConfig;

/// How many bytes of the request line to accept before giving up.
///
/// The request is a small JSON object; anything past this is a peer that is not speaking this
/// protocol, and reading forever on it would be a way to make the bridge allocate.
const MAX_REQUEST_LINE: u64 = 4 * 1024;

/// How long [`AcceptedStream`] keeps a connection's last handle alive after the stream
/// itself is dropped, waiting for the peer to close it.
///
/// Per [`iroh::endpoint::Connection`]'s own doc: once every handle to a connection — the
/// `Connection` itself, and every stream still live on it — has been dropped, the connection
/// closes at once, and "closing the connection immediately abandons efforts to deliver data
/// to the peer". `SendStream::finish` only *registers* that a stream is done; it is the
/// connection's own background driver that puts the bytes on the wire and waits for the
/// peer's ack. Every caller here follows the same shape — write a reply, then let the
/// stream drop — so without this, the drop can race that driver: the peer sees its
/// connection die instead of receiving the reply it was just sent. Keeping one more handle
/// open until the peer actually closes (or this timeout elapses) is iroh's own fix, lifted
/// from its `listen` example.
const ACCEPT_CLOSE_GRACE: Duration = Duration::from_secs(5);

/// An inbound stream, plus the connection it came from.
///
/// [`IrohTransport::accept`] hands this out instead of the bare joined stream it used to:
/// see [`ACCEPT_CLOSE_GRACE`] for why a connection must outlive every stream that answered
/// on it.
struct AcceptedStream {
    io: tokio::io::Join<::iroh::endpoint::RecvStream, ::iroh::endpoint::SendStream>,
    conn: ::iroh::endpoint::Connection,
}

impl AsyncRead for AcceptedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for AcceptedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

impl Drop for AcceptedStream {
    fn drop(&mut self) {
        // `io`'s streams are each a handle on this same connection and are about to drop
        // right along with this clone, so without it this would be the last handle —
        // exactly the case this type exists to avoid. The wait runs in the background:
        // `Drop::drop` cannot be `async`, and nothing here needs to block on it — the point
        // is only to give the connection's driver a chance to run before every handle that
        // kept it alive is finally gone.
        let conn = self.conn.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(ACCEPT_CLOSE_GRACE, conn.closed()).await;
        });
    }
}

/// A device's node id and the addresses that reach it.
///
/// This is what one device hands another out of band — the pairing flow this plan leaves for
/// later — so that the other can dial it without a discovery service. It deliberately names
/// nothing from iroh: a caller shares it, stores it, or prints it without linking iroh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAddr {
    /// The device's node id.
    pub node_id: String,
    /// The socket addresses this device can be reached at, as `host:port`.
    pub addrs: Vec<String>,
    /// The relay URLs this device is reachable through.
    ///
    /// Kept apart from `addrs` because they are a different kind of address: a peer with no
    /// direct path can only be dialed through one of these, and a device built relay-only
    /// has nothing else to offer.
    pub relay_urls: Vec<String>,
}

/// This host's endpoint on the network.
pub struct IrohTransport {
    endpoint: Endpoint,
    node_id: String,
    /// Addresses learned out of band, so a peer can be dialed without a discovery service.
    known: MemoryLookup,
}

impl IrohTransport {
    /// Bind an endpoint, loading or creating the secret key at `key_path`.
    ///
    /// `net` decides what the endpoint offers and uses: `discovery` turns on iroh's address
    /// lookup services, and the relay set is the host's own configuration merged with
    /// `workgroup`'s published one — see [`relays`](crate::relays) for how the two files
    /// combine. A caller that has already merged them (a test, mostly) uses
    /// [`IrohTransport::new`] directly.
    pub async fn new_with_relays(
        key_path: &Path,
        net: &NetConfig,
        workgroup: Option<&crate::workgroup::Workgroup>,
    ) -> Result<IrohTransport> {
        let config = crate::relays(net, workgroup)?;
        Self::new(key_path, net, &config).await
    }

    /// Bind an endpoint over an already-resolved relay configuration.
    ///
    /// `net` decides what the endpoint offers and uses: `discovery` turns on iroh's address
    /// lookup services, and `relays` is taken as given — an empty list with
    /// `use_default: false` disables relays entirely, which is what a test or a fully local
    /// host wants.
    pub async fn new(
        key_path: &Path,
        net: &NetConfig,
        relays: &RelayConfig,
    ) -> Result<IrohTransport> {
        Self::bind(key_path, net, relays, false).await
    }

    /// Bind an endpoint with no direct paths: every connection goes through a relay.
    ///
    /// Direct connections are what iroh prefers whenever it can find a path, so an endpoint
    /// built the ordinary way may reach a peer without the relay ever carrying a byte — which
    /// makes "does the relay work" untestable. Removing the IP transports leaves the relay as
    /// the only path, and a stream that arrives proves the relay carried it.
    ///
    /// `relays` must name at least one relay with `use_default: false`: an endpoint with
    /// neither a direct path nor a relay can reach nobody.
    pub async fn new_relay_only(
        key_path: &Path,
        net: &NetConfig,
        relays: &RelayConfig,
    ) -> Result<IrohTransport> {
        Self::bind(key_path, net, relays, true).await
    }

    /// Bind an endpoint, `relay_only` deciding whether direct paths are offered.
    async fn bind(
        key_path: &Path,
        net: &NetConfig,
        relays: &RelayConfig,
        relay_only: bool,
    ) -> Result<IrohTransport> {
        let secret = load_or_create_key(key_path)?;
        let known = MemoryLookup::new();

        // `Minimal` rather than `N0`: it picks the crypto provider, and nothing else. Every
        // other thing `N0` turns on is what `net` is here to decide.
        // Three protocols on one endpoint: the data plane and, each on its own ALPN, pairing and hello.
        // They are listed together because iroh accepts per endpoint; which one a
        // connection spoke is reported by `accept`, and the two are gated differently.
        let alpns = vec![ALPN.to_vec(), PAIR_ALPN.to_vec(), HELLO_ALPN.to_vec()];
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(alpns)
            .address_lookup(known.clone());
        if net.discovery {
            builder = builder
                .address_lookup(PkarrPublisher::n0_dns())
                .address_lookup(PkarrResolver::n0_dns())
                .address_lookup(DnsAddressLookup::n0_dns());
        }

        if relay_only {
            // No IP transport is registered at all, so there is no direct path to find or
            // fall back to: the relay is the only way out, and the only way in.
            builder = builder.clear_ip_transports();
        }
        let endpoint = builder
            .relay_mode(relay_mode(relays)?)
            .bind()
            .await
            .map_err(|e| Error::Peer(format!("could not bind the endpoint: {e}")))?;
        let node_id = endpoint.id().to_string();

        Ok(IrohTransport {
            endpoint,
            node_id,
            known,
        })
    }

    /// This device's node id and addresses, for handing to a peer out of band.
    ///
    /// Fails while the endpoint has nothing to share: an address with no transports on it
    /// would tell a peer to dial a device it cannot reach, and a pairing flow that shared one
    /// would be silently useless.
    pub async fn node_addr(&self) -> Result<NodeAddr> {
        let addr = self.endpoint.addr();
        if addr.is_empty() {
            return Err(Error::Peer(
                "this endpoint has no address to share yet".to_owned(),
            ));
        }
        Ok(NodeAddr {
            node_id: addr.id.to_string(),
            addrs: addr.ip_addrs().map(|addr| addr.to_string()).collect(),
            relay_urls: addr.relay_urls().map(|url| url.to_string()).collect(),
        })
    }

    /// Teach this endpoint how to reach `addr`, which is how a peer with discovery off is
    /// dialed.
    pub fn add_known_address(&self, addr: &NodeAddr) -> Result<()> {
        let id: EndpointId = addr
            .node_id
            .parse()
            .map_err(|e| Error::Config(format!("{}: not a node id: {e}", addr.node_id)))?;
        let mut transports = Vec::with_capacity(addr.addrs.len());
        for address in &addr.addrs {
            let socket: SocketAddr = address
                .parse()
                .map_err(|e| Error::Config(format!("{address}: not a socket address: {e}")))?;
            transports.push(TransportAddr::Ip(socket));
        }
        for url in &addr.relay_urls {
            let url: RelayUrl = url
                .parse()
                .map_err(|e| Error::Config(format!("{url}: not a relay URL: {e}")))?;
            transports.push(TransportAddr::Relay(url));
        }
        self.known
            .add_endpoint_info(EndpointAddr::from_parts(id, transports));
        Ok(())
    }
}

#[async_trait::async_trait]
impl PeerTransport for IrohTransport {
    async fn open(&self, node_id: &str, workspace_id: GrainId) -> Result<BoxedStream> {
        let id: EndpointId = node_id
            .parse()
            .map_err(|e| Error::Peer(format!("{node_id}: not a node id: {e}")))?;
        let conn = self
            .endpoint
            .connect(id, ALPN)
            .await
            .map_err(|e| Error::Peer(format!("could not reach {node_id}: {e}")))?;
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Peer(format!("could not open a stream to {node_id}: {e}")))?;

        // The request travels ahead of the payload, so the far side knows what was asked for
        // without looking inside the stream. Same framing as the data plane's header: one
        // JSON object and a newline.
        let request = StreamRequest {
            workspace_id,
            node_id: self.node_id.clone(),
        };
        let mut line = serde_json::to_vec(&request).map_err(|e| Error::Protocol(e.to_string()))?;
        line.push(b'\n');
        send.write_all(&line)
            .await
            .map_err(|e| Error::Peer(format!("could not send the request to {node_id}: {e}")))?;
        send.flush().await.map_err(|e| Error::Peer(e.to_string()))?;

        // The two halves keep the connection alive between them, so the caller may keep this
        // for as long as it likes.
        Ok(Box::new(tokio::io::join(recv, send)))
    }

    async fn open_pairing(&self, node_addr: &[u8]) -> Result<BoxedStream> {
        let addr: ::iroh::EndpointAddr = postcard::from_bytes(node_addr)
            .map_err(|e| Error::Peer(format!("the ticket's address is unreadable: {e}")))?;
        let conn = self
            .endpoint
            .connect(addr, PAIR_ALPN)
            .await
            .map_err(|e| Error::Peer(format!("could not reach the inviter: {e}")))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Peer(format!("could not open a pairing stream: {e}")))?;
        Ok(Box::new(tokio::io::join(recv, send)))
    }

    async fn open_hello(&self, node_id: &str) -> Result<BoxedStream> {
        let id: EndpointId = node_id
            .parse()
            .map_err(|e| Error::Peer(format!("{node_id}: not a node id: {e}")))?;
        let conn = self
            .endpoint
            .connect(id, HELLO_ALPN)
            .await
            .map_err(|e| Error::Peer(format!("could not reach {node_id}: {e}")))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Peer(format!("could not open a hello stream to {node_id}: {e}")))?;
        Ok(Box::new(tokio::io::join(recv, send)))
    }

    async fn accept(&self) -> Result<Inbound> {
        loop {
            let Some(incoming) = self.endpoint.accept().await else {
                return Err(Error::Peer("the endpoint is closed".to_owned()));
            };
            // Any host on the network may send a packet here, so a failed handshake or a
            // stream that never arrives must not end the loop: it is one caller going away,
            // not this bridge stopping.
            let mut accepting = match incoming.accept() {
                Ok(accepting) => accepting,
                Err(err) => {
                    tracing::debug!("a peer could not be accepted: {err}");
                    continue;
                }
            };
            // Which ALPN the caller dialed decides what may arrive on the stream: a
            // workspace request on the pairing ALPN, or a pairing exchange on the data
            // ALPN, is a peer that is not speaking this protocol. Reading the ALPN first is
            // also what keeps a pairing connection out of the code that reads a workspace
            // request line.
            let alpn = match accepting.alpn().await {
                Ok(alpn) => alpn,
                Err(err) => {
                    tracing::debug!("a peer's protocol could not be read: {err}");
                    continue;
                }
            };
            let conn = match accepting.await {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::debug!("a peer's handshake failed: {err}");
                    continue;
                }
            };
            let from = conn.remote_id();
            let (send, recv) = match conn.accept_bi().await {
                Ok(halves) => halves,
                Err(err) => {
                    tracing::debug!(peer = %from, "a peer opened no stream: {err}");
                    continue;
                }
            };
            if alpn == PAIR_ALPN {
                // The pairing gate is the invite secret, not the ledger, and the ledger
                // lookup `authorize` runs would fail every joiner by definition. This is
                // the one connection that arrives before membership exists.
                return Ok(Inbound::Pairing(
                    from.to_string(),
                    Box::new(AcceptedStream {
                        io: tokio::io::join(recv, send),
                        conn,
                    }),
                ));
            }
            if alpn == HELLO_ALPN {
                return Ok(Inbound::Hello(
                    from.to_string(),
                    Box::new(AcceptedStream {
                        io: tokio::io::join(recv, send),
                        conn,
                    }),
                ));
            }
            let (request, recv) = match read_request(recv).await {
                Ok(read) => read,
                Err(err) => {
                    tracing::debug!(peer = %from, "a peer sent an unreadable request: {err}");
                    continue;
                }
            };
            if request.node_id != from.to_string() {
                // Reported, not enforced: this transport reports who called, and the bridge
                // decides what a disagreement means. The name below is the authenticated one,
                // because that is the identity the far side cannot forge.
                tracing::debug!(
                    authenticated = %from,
                    claimed = %request.node_id,
                    "a peer's request named a different node id than the one that called"
                );
            }
            return Ok(Inbound::Workspace(
                from.to_string(),
                request.workspace_id,
                // `read_request` split the request line off `recv`; put the halves back
                // together for the caller, which sees one stream starting at the payload.
                Box::new(AcceptedStream {
                    io: tokio::io::join(recv, send),
                    conn,
                }),
            ));
        }
    }

    fn node_id(&self) -> String {
        self.node_id.clone()
    }

    fn is_connected(&self, node_id: &str) -> bool {
        // Whether a peer is reachable is the carrier's fact, and iroh's answer is the
        // paths it is *actively* using to that endpoint right now. `remote_info` is a
        // snapshot kept for a while after a peer goes away, so an `Inactive` entry alone
        // proves nothing: only an active path — a QUIC path in use by at least one live
        // connection, or the relay the endpoint is talking through — counts as connected.
        // A peer that has never spoken to this host has no entry at all, and is not
        // connected, which is also what the default says.
        let Ok(id) = self.endpoint_id(node_id) else {
            return false;
        };
        // The trait method is sync, and `remote_info` is async, but it is answered
        // directly from the endpoint's actor state — no network round trip — so a
        // blocking wait for the actor's one-shot is what "ask iroh" means here.
        match self.info_of(id) {
            None => false,
            Some(info) => info
                .addrs()
                .any(|a| matches!(a.usage(), ::iroh::endpoint::TransportAddrUsage::Active)),
        }
    }

    fn ticket_addr(&self) -> Result<Vec<u8>> {
        // The whole `EndpointAddr`, not just the id: a ticket that named no address would
        // send the joiner to a discovery service it may not have (relays and discovery are
        // both configurable off), and pairing must work on a host with neither.
        let addr = self.endpoint.addr();
        if addr.is_empty() {
            return Err(Error::Peer(
                "this endpoint has no address to put in a ticket yet".to_owned(),
            ));
        }
        postcard::to_stdvec(&addr)
            .map_err(|e| Error::Peer(format!("could not encode this host's address: {e}")))
    }
}

impl IrohTransport {
    /// The endpoint id behind `node_id`, if it is one.
    fn endpoint_id(&self, node_id: &str) -> Result<::iroh::EndpointId> {
        node_id
            .parse()
            .map_err(|e| Error::Peer(format!("{node_id}: not a node id: {e}")))
    }

    /// What iroh currently knows about `id`, fetched synchronously from its actor.
    ///
    /// `remote_info` is async because it asks the remote-state actor over a oneshot; the
    /// answer is ready immediately — the actor holds it in memory — so a short block on
    /// the calling thread is the cheapest way to serve a sync trait method. How that
    /// block is done depends on the runtime this call runs on: a multi-thread runtime's
    /// worker parks its task with [`tokio::task::block_in_place`], a current-thread
    /// runtime cannot do that (the answer would never be driven), so its handle's own
    /// `block_on` runs the future to completion, and a call from outside any runtime
    /// builds a throwaway one. The one-shot is answered as soon as the actor is
    /// scheduled, so every path here returns in microseconds; none of them waits on the
    /// network.
    fn info_of(&self, id: ::iroh::EndpointId) -> Option<::iroh::endpoint::RemoteInfo> {
        let ask = async { self.endpoint.remote_info(id).await };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => match handle.runtime_flavor() {
                tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| handle.block_on(ask))
                }
                // `RuntimeFlavor` is `#[non_exhaustive]`; anything that is not the
                // multi-thread runtime treats like the current-thread one.
                _ => handle.block_on(ask),
            },
            Err(_) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(ask)
            }
        }
    }
}

/// Read the request line, leaving the stream positioned at the first byte of the payload.
async fn read_request(
    mut recv: ::iroh::endpoint::RecvStream,
) -> Result<(StreamRequest, ::iroh::endpoint::RecvStream)> {
    // Capacity 1: a larger buffer would read past the newline and swallow payload bytes the
    // caller is about to be handed.
    let mut reader = BufReader::with_capacity(1, &mut recv);
    let mut line = String::new();
    // `read_line` grows its buffer until it sees a newline, so it is bounded here rather than
    // checked afterwards: a peer that never sends one must not be able to make the bridge
    // allocate without limit. One byte over, so a line that fits exactly still reads.
    let read = (&mut reader)
        .take(MAX_REQUEST_LINE + 1)
        .read_line(&mut line)
        .await
        .map_err(|e| Error::Protocol(format!("could not read the request line: {e}")))?;
    if read == 0 {
        return Err(Error::Protocol("the peer sent no request line".to_owned()));
    }
    if read as u64 > MAX_REQUEST_LINE {
        return Err(Error::Protocol(format!(
            "the request line is longer than {MAX_REQUEST_LINE} bytes"
        )));
    }
    let request = serde_json::from_str(line.trim())
        .map_err(|e| Error::Protocol(format!("bad peer request: {e}")))?;
    Ok((request, recv))
}

/// The relay mode [`RelayConfig`] asks for.
///
/// A custom relay map **replaces** iroh's relay set rather than adding to it, so with
/// `use_default` on, the public relays are folded back in first. The map is keyed by URL,
/// which is what keeps this fold and the merge in [`relays`](crate::relays) from producing
/// duplicates. Naming no relay is not an error either: a host may want direct connections
/// only, and an endpoint told "no relays" gets [`RelayMode::Disabled`].
fn relay_mode(config: &RelayConfig) -> Result<RelayMode> {
    if !config.use_default && config.urls.is_empty() {
        return Ok(RelayMode::Disabled);
    }
    let map = if config.use_default {
        default_relay_map()
    } else {
        RelayMap::empty()
    };
    for url in &config.urls {
        let url: RelayUrl = url
            .parse()
            .map_err(|e| Error::Config(format!("{url}: not a relay URL: {e}")))?;
        map.insert(
            url.clone(),
            Arc::new(::iroh::RelayConfig::from(url.clone())),
        );
    }
    Ok(RelayMode::Custom(map))
}

/// Read the secret key, or create one at `0600`.
///
/// This file **is** the device's identity: losing it means rejoining every workgroup as a new
/// device, so it is created once and never regenerated on a parse failure.
fn load_or_create_key(path: &Path) -> Result<SecretKey> {
    match std::fs::read(path) {
        Ok(bytes) => parse_key(path, &bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes)
                .map_err(|e| Error::Config(format!("no system random source: {e}")))?;
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            match create_private(path, &bytes)? {
                Created::Yes => Ok(SecretKey::from_bytes(&bytes)),
                // Another process won the race and wrote *its* key. Use that one, not the
                // bytes generated here: the file is the identity, and two processes that
                // disagreed about it would each think the other was a stranger.
                Created::AlreadyExisted => load_or_create_key(path),
            }
        }
        Err(e) => Err(Error::Io(e)),
    }
}

/// This host's node id, from the key at `key_path`, creating the key if it is absent.
///
/// Derived without binding an endpoint, which is what a one-shot command needs: founding a
/// workgroup records this host's node id, and that happens before — sometimes long before —
/// a bridge is running to ask.
pub fn load_or_create_node_id(key_path: &Path) -> Result<String> {
    Ok(load_or_create_key(key_path)?.public().to_string())
}

/// Interpret the bytes of a key file.
fn parse_key(path: &Path, bytes: &[u8]) -> Result<SecretKey> {
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        Error::Config(format!(
            "{}: a node key is 32 bytes, found {}; move it aside rather than \
             letting a new identity be minted",
            path.display(),
            bytes.len()
        ))
    })?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// Whether [`create_private`] wrote the file or found one already there.
#[derive(Debug, PartialEq, Eq)]
enum Created {
    Yes,
    AlreadyExisted,
}

/// Write a new key file that nobody else can read.
///
/// `create_new`, so a key that appeared between the read and here is never overwritten: the
/// device that already holds an identity keeps it.
fn create_private(path: &Path, bytes: &[u8; 32]) -> Result<Created> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The mode is masked by the umask, so it is set again below: the key must be private
        // whatever the user's umask is.
        options.mode(0o600);
    }

    match options.open(path) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.flush()?;
        }
        // Another process created the identity first.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(Created::AlreadyExisted);
        }
        Err(e) => return Err(Error::Io(e)),
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(Created::Yes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_relays_means_no_relay_transport() {
        let config = crate::relay::RelayConfig {
            urls: vec![],
            use_default: false,
        };
        assert!(matches!(relay_mode(&config).unwrap(), RelayMode::Disabled));
    }

    #[test]
    fn a_named_relay_is_used_and_a_bad_one_is_refused() {
        let config = crate::relay::RelayConfig {
            urls: vec!["https://relay.example/".to_owned()],
            use_default: false,
        };
        assert!(matches!(relay_mode(&config).unwrap(), RelayMode::Custom(_)));

        let config = crate::relay::RelayConfig {
            urls: vec!["not a url".to_owned()],
            use_default: false,
        };
        let err = relay_mode(&config).unwrap_err();
        assert!(err.to_string().contains("not a url"), "{err}");
    }

    #[test]
    fn the_public_relays_are_folded_back_into_a_custom_map() {
        // `RelayMode::Custom` replaces iroh's relay set rather than adding to it, so
        // `use_default` must fold the public relays in itself or they would be lost.
        let config = crate::relay::RelayConfig {
            urls: vec!["https://relay.example/".to_owned()],
            use_default: true,
        };
        let mode = relay_mode(&config).unwrap();
        let RelayMode::Custom(map) = mode else {
            panic!("expected a custom relay map");
        };
        let urls = map.urls::<Vec<::iroh::RelayUrl>>();
        assert!(urls.contains(&"https://relay.example/".parse().unwrap()));
        assert!(
            urls.len() > 1,
            "the public relays must be in the map too: {urls:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_node_id_follows_from_the_key_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("node.key");

        // Derived without an endpoint, and stable across calls.
        let id = load_or_create_node_id(&path).unwrap();
        assert_eq!(
            id.len(),
            64,
            "a node id is 64 lowercase hex characters: {id}"
        );
        assert_eq!(load_or_create_node_id(&path).unwrap(), id);

        // The same identity the transport binds.
        let net = NetConfig {
            wake_on_sync: false,
            discovery: false,
            relays: vec![],
            use_default_relays: false,
            ..NetConfig::default()
        };
        let transport = IrohTransport::new(&path, &net, &crate::relays(&net, None).unwrap())
            .await
            .unwrap();
        assert_eq!(transport.node_id(), id);
    }

    #[test]
    fn a_key_of_the_wrong_length_is_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("node.key");
        std::fs::write(&path, b"short").unwrap();

        let err = load_or_create_key(&path).unwrap_err();
        assert!(err.to_string().contains("32 bytes"), "{err}");
        // The bytes are still there: a damaged key is a human decision, not a fresh identity.
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
    }

    #[test]
    fn an_existing_key_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("node.key");

        let first = load_or_create_key(&path).unwrap();
        assert_eq!(
            create_private(&path, &[7u8; 32]).unwrap(),
            Created::AlreadyExisted
        );

        // Loading again gives the identity that is on disk, not the one just offered.
        let second = load_or_create_key(&path).unwrap();
        assert_eq!(second.to_bytes(), first.to_bytes());
        assert_eq!(std::fs::read(&path).unwrap(), first.to_bytes());
    }
}
