//! Two complete hosts, converging.
//!
//! The first test in the series that exercises the finished shape: client → app server →
//! bridge → peer bridge → peer app server → files. Everything below is the real thing — two
//! bridges, two app servers, the loopback transport in place of a network — so a claim here
//! is a claim about the architecture, not about a fixture.

use std::path::PathBuf;

use sapphire_backend::protocol as proto;
use sapphire_framework_bridge::LoopbackNetwork;

mod common;
use common::{Host, start_host};

/// Enable sync on both hosts for one workspace and let them meet.
async fn synced_pair(net: &LoopbackNetwork) -> (Host, Host) {
    let a = start_host(net, common::NODE_A, "host-a").await;
    let b = start_host(net, common::NODE_B, "host-b").await;
    common::introduce(&a, &b);
    let _: proto::SyncEnableResult = a
        .client
        .call(proto::SYNC_ENABLE, serde_json::json!({}))
        .await
        .unwrap();
    let _: proto::SyncEnableResult = b
        .client
        .call(proto::SYNC_ENABLE, serde_json::json!({}))
        .await
        .unwrap();
    (a, b)
}

/// Wait for `path` to exist on `host`, or fail. Never sleeps blindly: polls a condition.
async fn await_file(host: &Host, rel: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(host.ws.join(rel)) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{rel} never arrived on {}",
            host.ws.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_made_through_the_ipc_client_is_synced() {
    let net = LoopbackNetwork::new();
    let (a, b) = synced_pair(&net).await;

    let _: proto::Ack = a
        .client
        .call(
            proto::WRITE_FILE,
            proto::ContentParams {
                path: PathBuf::from("note.md"),
                content: "from host a".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(await_file(&b, "note.md").await, "from host a");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_made_outside_the_server_is_synced() {
    let net = LoopbackNetwork::new();
    let (a, b) = synced_pair(&net).await;

    // As if the user had opened an editor.
    std::fs::write(a.ws.join("by-hand.md"), "typed directly").unwrap();

    assert_eq!(await_file(&b, "by-hand.md").await, "typed directly");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_search_index_on_the_receiving_host_finds_the_new_file() {
    let net = LoopbackNetwork::new();
    let (a, b) = synced_pair(&net).await;

    let _: proto::Ack = a
        .client
        .call(
            proto::WRITE_FILE,
            proto::ContentParams {
                path: PathBuf::from("searchable.md"),
                content: "the quick brown fox".into(),
            },
        )
        .await
        .unwrap();
    await_file(&b, "searchable.md").await;

    // The receiving app server applied the change, so the file and its index moved together.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let hits: proto::SearchResult = b
            .client
            .call(
                proto::SEARCH,
                proto::SearchParams {
                    query: "brown".into(),
                    limit: 10,
                    mode: sapphire_backend::SearchMode::Fts,
                },
            )
            .await
            .unwrap();
        if hits.hits.iter().any(|h| h.path.ends_with("searchable.md")) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a synced file must become searchable without a manual reindex"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_was_offline_catches_up_when_it_returns() {
    let net = LoopbackNetwork::new();
    let (a, mut b) = synced_pair(&net).await;

    // B goes away.
    b.stop().await;

    let _: proto::Ack = a
        .client
        .call(
            proto::WRITE_FILE,
            proto::ContentParams {
                path: PathBuf::from("while-away.md"),
                content: "written while b was down".into(),
            },
        )
        .await
        .unwrap();

    // B comes back with the same directories.
    let b = b.restart(&net).await;
    let _: proto::SyncEnableResult = b
        .client
        .call(proto::SYNC_ENABLE, serde_json::json!({}))
        .await
        .unwrap();

    assert_eq!(
        await_file(&b, "while-away.md").await,
        "written while b was down"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_bridge_down_the_app_server_still_serves_files() {
    let net = LoopbackNetwork::new();
    let (mut a, _b) = synced_pair(&net).await;

    a.stop_bridge().await;

    let _: proto::Ack = a
        .client
        .call(
            proto::WRITE_FILE,
            proto::ContentParams {
                path: PathBuf::from("offline.md"),
                content: "still works".into(),
            },
        )
        .await
        .expect("the app server must not depend on the bridge to serve files");

    let status: proto::SyncStatusResult = a
        .client
        .call(proto::SYNC_STATUS, serde_json::json!({}))
        .await
        .expect("status must answer even with the bridge down");
    assert!(!status.bridge_available);
}
