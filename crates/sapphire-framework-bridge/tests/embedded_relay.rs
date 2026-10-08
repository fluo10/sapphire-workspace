//! A relay server running inside the bridge's own process.
//!
//! Two endpoints meet here through the relay and nowhere else: they are built relay-only, so
//! no direct path exists, and the bytes that arrive on the far side can only have come
//! through the relay this test started. That is what says the feature works rather than
//! merely starts.

#![cfg(feature = "embedded-relay")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use grain_id::GrainId;
use sapphire_framework_bridge::{
    EmbeddedRelay, EmbeddedRelayConfig, Inbound, IrohTransport, NetConfig, NodeAddr, PeerTransport,
    RelayConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a test waits for something that should happen quickly but does not happen
/// instantly — a relay connection coming up, or a peer accepting.
const PATIENCE: Duration = Duration::from_secs(30);

/// A relay on loopback, on an ephemeral port, without TLS.
///
/// The loopback address is what the relay refuses unless it is asked for: on a real host it
/// would be an address no peer can reach.
fn loopback(port: u16) -> EmbeddedRelayConfig {
    EmbeddedRelayConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        hostname: "localhost".into(),
        allow_loopback: true,
        ..EmbeddedRelayConfig::default()
    }
}

/// A host that neither discovers peers nor uses any relay but the one passed in.
fn offline() -> NetConfig {
    NetConfig {
        wake_on_sync: false,
        discovery: false,
        relays: vec![],
        use_default_relays: false,
        ..NetConfig::default()
    }
}

/// The address of `transport`, once its relay connection is up.
///
/// A relay-only endpoint has no direct address to share, so until it has connected to the
/// relay there is nothing for a peer to dial and `node_addr` says so. Waiting here keeps that
/// from being read as "the relay does not work".
async fn relay_address(transport: &IrohTransport) -> NodeAddr {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Ok(addr) = transport.node_addr().await
            && !addr.relay_urls.is_empty()
        {
            return addr;
        }
        assert!(
            Instant::now() < deadline,
            "the endpoint never announced the relay"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_starts_and_reports_its_url() {
    let relay = EmbeddedRelay::start(&loopback(0)).await.unwrap();
    assert!(relay.url().starts_with("http"), "{}", relay.url());
    relay.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loopback_address_is_refused_unless_it_is_asked_for() {
    let mut config = loopback(0);
    config.allow_loopback = false;
    let err = EmbeddedRelay::start(&config).await.unwrap_err();
    assert!(
        err.to_string().contains("reachable"),
        "the message must say what is wrong: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_hostname_is_refused() {
    let mut config = loopback(0);
    config.hostname = String::new();
    let err = EmbeddedRelay::start(&config).await.unwrap_err();
    assert!(err.to_string().contains("hostname"), "{err}");
}

/// A certificate without its key is a relay nobody can complete a handshake with, so it is
/// refused rather than started as something broken.
#[tokio::test(flavor = "multi_thread")]
async fn half_a_certificate_is_refused() {
    let mut config = loopback(0);
    config.tls.cert_path = Some("/nowhere/cert.pem".into());
    let err = EmbeddedRelay::start(&config).await.unwrap_err();
    assert!(err.to_string().contains("key"), "{err}");
}

/// Two endpoints meet through the embedded relay.
///
/// Both are built relay-only, so no direct path exists to fall back on, and each is taught
/// only the other's relay address. The bytes that cross therefore crossed the relay.
#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_meet_through_the_embedded_relay() {
    let relay = EmbeddedRelay::start(&loopback(0)).await.unwrap();

    // The relay the endpoints may use, through the same `RelayConfig` the bridge feeds its
    // endpoints from: an embedded relay is not a second way of configuring a relay.
    let relays = RelayConfig {
        urls: vec![relay.url()],
        use_default: false,
    };

    let tmp = tempfile::tempdir().unwrap();
    let a = IrohTransport::new_relay_only(&tmp.path().join("a.key"), &offline(), &relays)
        .await
        .unwrap();
    let b = Arc::new(
        IrohTransport::new_relay_only(&tmp.path().join("b.key"), &offline(), &relays)
            .await
            .unwrap(),
    );

    let b_node_id = b.node_id();
    let a_node_id = a.node_id();
    let b_addr = relay_address(&b).await;
    // A is taught nothing but the relay B sits behind.
    a.add_known_address(&b_addr).unwrap();

    let ws = GrainId::random();
    let accepting = Arc::clone(&b);
    let accept = tokio::spawn(async move { accepting.accept().await });
    let mut opened = a.open(&b_node_id, ws).await.unwrap();

    let inbound = tokio::time::timeout(PATIENCE, accept)
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
    assert_eq!(&buf, b"ping", "the relay must carry the payload");

    // …and the far side answers, on the same stream.
    accepted.write_all(b"pong").await.unwrap();
    let mut back = [0u8; 4];
    opened.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"pong");

    drop(opened);
    drop(accepted);
    relay.stop().await;
}
