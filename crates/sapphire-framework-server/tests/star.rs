//! The star topology, end to end.
//!
//! Bridges elect a designated and a backup device per workspace from the priorities their
//! Hellos carry. An app server that holds neither role syncs only with those two hubs; with
//! no candidates every device falls back to the mesh. These tests drive real hosts on one
//! loopback network through relay, takeover, no preemption, an election among a
//! workspace's own hosts, and the all-zero mesh fallback.

mod common;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{NODE_A, NODE_B, NODE_C, NODE_S};
use grain_id::GrainId;
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

/// The devices `host` holds an open live session to, right now.
async fn sessions(host: &common::Host) -> HashSet<GrainId> {
    host.runtime()
        .unwrap()
        .live_session_devices(&host.ws)
        .await
        .into_iter()
        .collect()
}

/// Wait until `host`'s open live sessions are exactly `want`.
async fn await_sessions(host: &common::Host, want: &HashSet<GrainId>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let open = sessions(host).await;
        if &open == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never held exactly the sessions {want:?}; it holds {open:?}",
            host.node_id()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until `host` holds an open live session to `peer`.
async fn await_session_to(host: &common::Host, peer: GrainId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let open = sessions(host).await;
        if open.contains(&peer) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never opened a session to {peer}; it holds {open:?}",
            host.node_id()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The designated device `host`'s bridge reports for its workspace, right now.
async fn designated(host: &common::Host) -> Option<GrainId> {
    let peers = host.bridge().peers().await.expect("peers");
    peers
        .roles_for(host.workspace_id().await)
        .and_then(|r| r.designated)
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
    let (a, s, b, c) = (&hosts[0], &hosts[1], &hosts[2], &hosts[3]);
    let (a_id, s_id) = (a.device_id().await, s.device_id().await);
    let (b_id, c_id) = (b.device_id().await, c.device_id().await);
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), a_id).await;

    // The star is applied once each non-hub talks to exactly the two hubs: any direct B–C
    // session a pre-election dial walk opened has been closed by then.
    let hubs: HashSet<GrainId> = [a_id, s_id].into_iter().collect();
    await_sessions(b, &hubs).await;
    await_sessions(c, &hubs).await;

    write(b, "via-hub.md", "x").await;

    assert_eq!(await_file(c, "via-hub.md").await, "x");
    // Checked at once: a stale direct session the edit could have ridden would still be
    // open, so its absence now means the edit came through a hub.
    assert!(
        !sessions(b).await.contains(&c_id),
        "B held a direct session to C"
    );
    assert!(
        !sessions(c).await.contains(&b_id),
        "C held a direct session to B"
    );
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

    // The spec's bound — no sync gap longer than one dial pass — is not enforced here; this
    // proves only that the takeover happens and sync then works through the new hub.
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
    common::enable_sync_after_restart(&a).await;

    // A, back and hearing the others, must first agree that S holds the role.
    let mut all: Vec<&common::Host> = hosts.iter().collect();
    all.push(&a);
    common::await_designated(&all, s_id).await;

    // Several dead intervals later, S still holds it. A fixed sleep on purpose: this proves
    // that something does *not* happen, and the check after it is one snapshot, not a poll
    // that could wait out a flip and back.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for host in &all {
        assert_eq!(
            designated(host).await,
            Some(s_id),
            "{} no longer reports S as designated",
            host.node_id()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_the_top_priority_device_does_not_host_elects_among_its_hosts() {
    // B and C share a workspace that A (priority 3) does not host. Their roles come only
    // from each other, so B and C are designated and backup — a Star{B, C} — and, being
    // the two hubs, they hold a session with each other.
    //
    // The harness gives each host one workspace, so the spec's scenario of A running a star
    // on another workspace at the same time is not built here. That cross-workspace
    // isolation is covered by the Elector unit test
    // `a_peer_not_hosting_the_workspace_is_not_a_candidate`.
    let net = LoopbackNetwork::new();
    let a = common::start_host_with_priority(&net, NODE_A, "host-a", 3).await;
    let b = common::start_host_with_priority(&net, NODE_B, "host-b", 1).await;
    let c = common::start_host_with_priority(&net, NODE_C, "host-c", 1).await;
    common::introduce_all(&[&a, &b, &c]); // ledgers
    common::introduce(&b, &c); // a workspace identity A does not share
    common::enable_sync(&b).await;
    common::enable_sync(&c).await;
    let a_id = a.device_id().await;
    let (b_id, c_id) = (b.device_id().await, c.device_id().await);
    let pair: HashSet<GrainId> = [b_id, c_id].into_iter().collect();

    // Both roles filled, on both hosts, and A never named in either — not even on the way.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut settled = true;
        for host in [&b, &c] {
            let peers = host.bridge().peers().await.expect("peers");
            let roles = peers.roles_for(host.workspace_id().await).cloned();
            let named: Vec<GrainId> = roles
                .iter()
                .flat_map(|r| [r.designated, r.backup])
                .flatten()
                .collect();
            assert!(
                !named.contains(&a_id),
                "{} elected A, which does not host the workspace: {roles:?}",
                host.node_id()
            );
            if named.iter().copied().collect::<HashSet<_>>() != pair {
                settled = false;
            }
        }
        if settled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B and C never became designated and backup"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    await_session_to(&b, c_id).await;
    await_session_to(&c, b_id).await;

    write(&b, "pair.md", "z").await;
    assert_eq!(await_file(&c, "pair.md").await, "z");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_every_priority_zero_the_mesh_is_used() {
    let net = LoopbackNetwork::new();
    let (a, s, b) = common::synced_triple(&net).await; // harness default: priority 0
    common::settle(&[&a, &s, &b]).await; // the mesh: everyone to everyone
    for host in [&a, &s, &b] {
        let status = host.runtime().unwrap().status(&host.ws).await;
        assert_eq!(
            status.topology,
            proto::Topology::Mesh,
            "{} is not in a mesh",
            host.node_id()
        );
    }
}
