//! Two iroh endpoints in one process, on localhost, with no relay and no discovery.
//!
//! These are the tests that say the real transport works: the loopback tests in `peer.rs`
//! prove the switchboard, not the network. Everything here stays offline — the endpoints are
//! built from an offline [`NetConfig`], so a machine with no route to a relay still runs them.

#![cfg(feature = "node")]

use std::sync::Arc;
use std::time::Duration;

use grain_id::GrainId;
use sapphire_framework_bridge::{Inbound, IrohTransport, NetConfig, PeerTransport, relays};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `Result::unwrap_err` wants the success type to be `Debug`, and a peer stream is not: it is
/// an arbitrary reader and writer. Unwrapping by hand keeps `PeerStream` free of a `Debug`
/// bound no transport should have to satisfy.
#[track_caller]
fn expect_err<T>(result: sapphire_framework_bridge::Result<T>) -> sapphire_framework_bridge::Error {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(err) => err,
    }
}

/// A host that neither discovers peers nor uses a relay: local addresses only.
fn offline() -> NetConfig {
    NetConfig {
        wake_on_sync: false,
        discovery: false,
        relays: vec![],
        use_default_relays: false,
        ..NetConfig::default()
    }
}

/// The transport for `net`, with its relay set already resolved the way the bridge does.
async fn transport(tmp: &tempfile::TempDir, name: &str, net: &NetConfig) -> IrohTransport {
    let config = relays(net, None).unwrap();
    IrohTransport::new(&tmp.path().join(name), net, &config)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_file_is_created_once_and_reused() {
    let tmp = tempfile::tempdir().unwrap();

    let first = transport(&tmp, "node.key", &offline()).await;
    let id = first.node_id();
    drop(first);

    let second = transport(&tmp, "node.key", &offline()).await;
    assert_eq!(second.node_id(), id, "the node id must survive a restart");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_key_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let key = tmp.path().join("node.key");
    let _ = transport(&tmp, "node.key", &offline()).await;
    let mode = std::fs::metadata(&key).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_peer_is_reported_connected_and_an_unknown_one_is_not() {
    // The status surface (`bridge.peers`, `status.json`, `device list`) gets its
    // connected answer from here, so the real transport must say who is up: iroh knows
    // the paths it is actively using, and a peer nothing has been exchanged with has no
    // entry at all.
    let tmp = tempfile::tempdir().unwrap();
    let a = transport(&tmp, "a.key", &offline()).await;
    let b = transport(&tmp, "b.key", &offline()).await;
    let b_node_id = b.node_id();
    let b_addr = b.node_addr().await.unwrap();
    a.add_known_address(&b_addr).unwrap();

    // Before anything is dialed, nobody is connected.
    assert!(
        !a.is_connected(&b_node_id),
        "a peer never dialed is not connected"
    );

    // One open in each direction — the shape every bridge conversation has — and B is up.
    let ws = GrainId::random();
    let accepting = tokio::spawn({
        let b = std::sync::Arc::new(b);
        async move { b.accept().await }
    });
    let opened = a.open(&b_node_id, ws).await.unwrap();
    let inbound = tokio::time::timeout(Duration::from_secs(10), accepting)
        .await
        .expect("the far side to accept")
        .unwrap()
        .unwrap();
    // The handshake is done on both ends: the connection exists, whatever becomes of the
    // streams on it. Held until the assertion below has been made, so neither side's
    // endpoint tears it down first.
    let _keep = (opened, inbound);
    // iroh's remote state settles when the path it is using is confirmed; the handshake
    // has already done that, but the actor hears of it asynchronously.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if a.is_connected(&b_node_id) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a peer with an active path must be reported connected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stranger = grain_id::GrainId::random().to_string();
    assert!(
        !a.is_connected(&stranger),
        "an unknown node id is not connected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_exchange_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let a = transport(&tmp, "a.key", &offline()).await;
    // `accept` borrows the transport, and the accepting side must outlive the call: the
    // endpoint is what keeps the connection up. This is the shape the bridge uses, too — one
    // `Arc<dyn PeerTransport>` shared by the loops.
    let b = Arc::new(transport(&tmp, "b.key", &offline()).await);

    // B's identity and address are needed before B is moved into the accepting task.
    let b_node_id = b.node_id();
    let b_addr = b.node_addr().await.unwrap();

    // Teach A where B is, since discovery is off: nothing else tells A how to reach B.
    a.add_known_address(&b_addr).unwrap();

    let ws = GrainId::random();
    let a_node_id = a.node_id();
    let accepting = Arc::clone(&b);
    let accept = tokio::spawn(async move { accepting.accept().await });
    let mut opened = a.open(&b_node_id, ws).await.unwrap();

    let inbound = tokio::time::timeout(Duration::from_secs(10), accept)
        .await
        .expect("the far side to accept")
        .unwrap()
        .unwrap();
    let (from, asked, mut accepted) = match inbound {
        Inbound::Workspace(from, asked, stream) => (from, asked, stream),
        Inbound::Pairing(..) => panic!("a workspace open arrived as a pairing stream"),
        Inbound::Hello(..) => panic!("a workspace open arrived as a hello stream"),
    };
    assert_eq!(from, a_node_id, "the far side must learn who called");
    assert_eq!(asked, ws, "the far side must learn what was asked for");

    opened.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    accepted.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");

    // And the far side can answer, on the same stream.
    accepted.write_all(b"pong").await.unwrap();
    let mut back = [0u8; 4];
    opened.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"pong");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_survives_the_accepting_side_dropping_the_stream_at_once() {
    // This is the shape every caller in this crate uses: write the last message, then let
    // the stream — and nothing else — go out of scope. Per `iroh::endpoint::Connection`'s
    // own doc, once every handle to a connection (the `Connection` itself, and every stream
    // still live on it) has dropped, the connection closes at once and "immediately abandons
    // efforts to deliver data to the peer" — discarding whatever was queued but not yet on
    // the wire. `accept` must keep the connection alive a little past the stream it hands
    // out, or the opener never sees what was just written to it.
    let tmp = tempfile::tempdir().unwrap();
    let a = transport(&tmp, "a.key", &offline()).await;
    let b = Arc::new(transport(&tmp, "b.key", &offline()).await);

    let b_node_id = b.node_id();
    let b_addr = b.node_addr().await.unwrap();
    a.add_known_address(&b_addr).unwrap();

    let ws = GrainId::random();
    let accepting = Arc::clone(&b);
    let accept = tokio::spawn(async move {
        let Inbound::Workspace(_, _, mut stream) = accepting.accept().await.unwrap() else {
            panic!("expected a workspace stream");
        };
        stream.write_all(b"reply").await.unwrap();
        stream.flush().await.unwrap();
        // Dropped here, at the end of the task — no shutdown, no linger, no wait for the
        // opener to finish reading.
    });

    let mut opened = a.open(&b_node_id, ws).await.unwrap();
    accept.await.unwrap();

    let mut buf = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(10), opened.read_exact(&mut buf))
        .await
        .expect("the read must not hang")
        .expect("the reply must survive the accepting side's drop");
    assert_eq!(&buf, b"reply");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_id_that_is_not_a_node_id_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let a = transport(&tmp, "a.key", &offline()).await;

    let err = expect_err(a.open("not-a-node-id", GrainId::random()).await);
    assert!(err.to_string().contains("not-a-node-id"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_address_that_is_not_an_address_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let a = transport(&tmp, "a.key", &offline()).await;

    let mut addr = a.node_addr().await.unwrap();
    addr.addrs = vec!["not-an-address".to_owned()];
    let err = expect_err(a.add_known_address(&addr));
    assert!(err.to_string().contains("not-an-address"), "{err}");
}
