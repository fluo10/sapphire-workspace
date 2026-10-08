//! The star topology, end to end.
//!
//! Bridges elect a designated and a backup device per workspace from the priorities their
//! Hellos carry. An app server that holds neither role syncs only with those two hubs; with
//! no candidates every device falls back to the mesh. These tests drive four real hosts on
//! one loopback network through relay, takeover, no preemption and both mesh fallbacks.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{NODE_A, NODE_B, NODE_C, NODE_S};
use sapphire_backend::protocol as proto;
use sapphire_framework_bridge::LoopbackNetwork;

async fn await_file(host: &common::Host, rel: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(host.ws.join(rel)) {
            return text;
        }
        assert!(std::time::Instant::now() < deadline, "{rel} never arrived");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn write(host: &common::Host, rel: &str, content: &str) {
    let _: proto::Ack = host
        .client
        .call(
            proto::WRITE_FILE,
            proto::ContentParams {
                ws: host.ws.clone(),
                path: PathBuf::from(rel),
                content: content.into(),
            },
        )
        .await
        .unwrap();
}

async fn await_no_session(host: &common::Host, peer: grain_id::GrainId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let open = host.runtime().unwrap().live_session_devices(&host.ws).await;
        if !open.contains(&peer) {
            return;
        }
        assert!(Instant::now() < deadline, "a session to {peer} stayed open");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_between_two_non_hubs_travels_through_the_hub() {
    let net = LoopbackNetwork::new();
    let hosts = common::star_hosts(
        &net,
        &[
            (NODE_A, "host-a", 3),
            (NODE_S, "host-s", 2),
            (NODE_B, "host-b", 1),
            (NODE_C, "host-c", 1),
        ],
    )
    .await;
    let (a, b, c) = (&hosts[0], &hosts[2], &hosts[3]);
    let ids = [
        a.device_id().await,
        b.device_id().await,
        c.device_id().await,
    ];
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), ids[0]).await;
    await_no_session(b, ids[2]).await;
    await_no_session(c, ids[1]).await;

    write(b, "via-hub.md", "x").await;

    assert_eq!(await_file(c, "via-hub.md").await, "x");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_backup_takes_over_when_the_designated_device_stops() {
    let net = LoopbackNetwork::new();
    let mut hosts = common::star_hosts(
        &net,
        &[
            (NODE_A, "host-a", 3),
            (NODE_S, "host-s", 2),
            (NODE_B, "host-b", 1),
            (NODE_C, "host-c", 1),
        ],
    )
    .await;
    let s_id = hosts[1].device_id().await;
    let a_id = hosts[0].device_id().await;
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), a_id).await;

    hosts[0].stop().await;
    let rest: Vec<&common::Host> = hosts[1..].iter().collect();
    common::await_designated(&rest, s_id).await;

    write(&hosts[2], "after.md", "y").await;
    assert_eq!(await_file(&hosts[3], "after.md").await, "y");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_returning_device_does_not_take_the_role_back() {
    let net = LoopbackNetwork::new();
    let mut hosts = common::star_hosts(
        &net,
        &[
            (NODE_A, "host-a", 3),
            (NODE_S, "host-s", 2),
            (NODE_B, "host-b", 1),
        ],
    )
    .await;
    let a_id = hosts[0].device_id().await;
    let s_id = hosts[1].device_id().await;
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), a_id).await;

    let mut a = hosts.remove(0);
    a.stop().await;
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), s_id).await;
    let a = a.restart(&net).await;
    // `restart` leaves sync disabled; enable it the way `star_hosts` does.
    common::enable_sync(&a).await;

    // Several dead intervals later, S still holds the role. A fixed sleep on purpose: this
    // proves that something does *not* happen.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut all: Vec<&common::Host> = hosts.iter().collect();
    all.push(&a);
    common::await_designated(&all, s_id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_the_hub_does_not_host_stays_a_mesh() {
    // B and C share a workspace that A (priority 3) does not host. Their roles come only
    // from each other, so B and C are designated and backup, and they hold a session.
    let net = LoopbackNetwork::new();
    let a = common::start_host_with_priority(&net, NODE_A, "host-a", 3).await;
    let b = common::start_host_with_priority(&net, NODE_B, "host-b", 1).await;
    let c = common::start_host_with_priority(&net, NODE_C, "host-c", 1).await;
    common::introduce_all(&[&a, &b, &c]); // ledgers
    common::introduce(&b, &c); // a workspace identity A does not share
    common::enable_sync(&b).await;
    common::enable_sync(&c).await;

    write(&b, "pair.md", "z").await;
    assert_eq!(await_file(&c, "pair.md").await, "z");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_every_priority_zero_the_mesh_is_used() {
    let net = LoopbackNetwork::new();
    let (a, s, b) = common::synced_triple(&net).await; // harness default: priority 0
    common::settle(&[&a, &s, &b]).await; // the mesh: everyone to everyone
    let status = a.runtime().unwrap().status(&a.ws).await;
    assert_eq!(status.topology, proto::Topology::Mesh);
}
