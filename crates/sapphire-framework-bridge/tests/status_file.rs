//! The running bridge keeps `status.json` current.
//!
//! Unit tests in `status.rs` pin the file's mechanics against a fixed source; this test
//! wires the writer the way production does and checks the file reports the facts a real
//! bridge has — devices, routes, workgroup.

mod common;

use common::*;
use grain_id::GrainId;
use sapphire_bridge_api::{ManagedBy, RegisterParams, WorkspaceRegistration};
use sapphire_framework_bridge::{LoopbackNetwork, STATUS_INTERVAL, StatusFile};
use std::sync::Arc;

/// A fixed snapshot as the source, so the test drives the writer exactly as the bridge
/// will: through [`StatusWriter::start`].
struct Fixed(StatusFile);

impl sapphire_framework_bridge::StatusSource for Fixed {
    fn snapshot(&self) -> StatusFile {
        self.0.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_bridge_writes_its_status_file() {
    let net = LoopbackNetwork::new();
    let a = start(&net, NODE_A, "host-a").await;

    // The bridge is running (`start` waits for its control endpoint), so the file must be
    // there with this host's facts in it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let path = a.dir.status_json();
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "a running bridge never wrote status.json"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let snapshot = sapphire_framework_bridge::StatusFile::load(&path)
        .unwrap()
        .expect("the file it just wrote");
    assert_eq!(snapshot.node_id, NODE_A);
    assert_eq!(snapshot.pid, std::process::id());
    assert_eq!(
        snapshot.workgroup.expect("a workgroup").devices,
        1,
        "the workgroup's one device"
    );
    assert_eq!(snapshot.peers.len(), 1, "the one device, as a peer");
    assert_eq!(snapshot.version, "0.0.0");

    // Repeated snapshots must all parse: every write is atomic.
    for _ in 0..3 {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let snapshot = sapphire_framework_bridge::StatusFile::load(&path)
            .unwrap()
            .expect("the snapshot stays");
        assert_eq!(snapshot.node_id, NODE_A);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn registering_a_workspace_reaches_the_status_file() {
    let net = LoopbackNetwork::new();
    let a = start(&net, NODE_A, "host-a").await;
    let client = connect(&a).await;
    let ws = GrainId::random();
    client
        .register(RegisterParams {
            app_name: APP.into(),
            exe_path: std::env::current_exe().unwrap(),
            managed_by: ManagedBy::Spawned,
            workspaces: vec![WorkspaceRegistration {
                workspace_id: ws,
                root: "/workspace".into(),
            }],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let snapshot = sapphire_framework_bridge::StatusFile::load(&a.dir.status_json())
            .unwrap()
            .expect("a running bridge wrote status.json");
        if snapshot.routes.iter().any(|r| r.workspace_id == ws) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a registration never reached status.json"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// A registration that reaches the file must reach it by way of a change, not the interval:
/// the poll is one second, so a forced wait of the full interval would make the test pass
/// even if the writer only ever wrote on the tick.
#[tokio::test(flavor = "multi_thread")]
async fn a_registration_reaches_the_status_file_faster_than_the_tick() {
    let net = LoopbackNetwork::new();
    let a = start(&net, NODE_A, "host-a").await;
    let client = connect(&a).await;
    let ws = GrainId::random();
    client
        .register(RegisterParams {
            app_name: APP.into(),
            exe_path: std::env::current_exe().unwrap(),
            managed_by: ManagedBy::Spawned,
            workspaces: vec![WorkspaceRegistration {
                workspace_id: ws,
                root: "/workspace".into(),
            }],
        })
        .await
        .unwrap();

    let began = std::time::Instant::now();
    let deadline = std::time::Duration::from_millis(STATUS_INTERVAL.as_millis() as u64);
    loop {
        let snapshot = sapphire_framework_bridge::StatusFile::load(&a.dir.status_json())
            .unwrap()
            .expect("a running bridge wrote status.json");
        if snapshot.routes.iter().any(|r| r.workspace_id == ws) {
            break;
        }
        assert!(
            began.elapsed() < deadline,
            "the registration took longer than a full interval; the change path may not be the one writing"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    client.unregister(ws).await.unwrap();
}

/// The writer is also usable directly, as `test-util` consumers will.
#[tokio::test(flavor = "multi_thread")]
async fn the_writer_writes_to_the_bridge_directory_it_is_handed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("status.json");
    let snapshot = StatusFile {
        version: "9.9.9".into(),
        pid: 42,
        started_at: chrono::Utc::now(),
        node_id: "abc".repeat(21),
        workgroup: None,
        peers: vec![],
        routes: vec![],
        embedding: None,
        relays: vec!["https://relay.example".into()],
    };
    let writer =
        sapphire_framework_bridge::StatusWriter::start(path.clone(), Arc::new(Fixed(snapshot)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "no status.json was written"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let parsed = sapphire_framework_bridge::StatusFile::load(&path)
        .unwrap()
        .expect("the file was just written");
    assert_eq!(parsed.relays, vec!["https://relay.example".to_owned()]);
    writer.stop();
}
