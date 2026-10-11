//! Live propagation between app servers, including through a middle host.
//!
//! The claim under test is the app server's half of the "sync feels immediate" goal: a
//! commit made through one host's IPC reaches every other host over sessions that are
//! already open, a host in the middle forwards what it receives, and the two storm rules —
//! never back to the source, never what the peer already has — hold when three hosts are
//! connected in a triangle.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use sapphire_backend::protocol as proto;
use sapphire_framework_bridge::LoopbackNetwork;

/// How long the frame counters must hold still before a host counts as quiet.
const QUIET: Duration = Duration::from_secs(1);
/// How long to keep waiting for that quiet before calling it a loop.
const QUIET_DEADLINE: Duration = Duration::from_secs(30);
/// How long a quiet host is then watched for a loop to start.
const OBSERVE: Duration = Duration::from_secs(3);
/// How many writes each watched host may still make over [`OBSERVE`] before it is a loop.
///
/// This is a bound, not an equality, on purpose: the app servers keep up ambient traffic of
/// their own — the bridge's sweeps and the dial loops' retries, a handful of writes per
/// round — that has nothing to do with the edit under test and never stops entirely. What
/// the test rules out is a *storm*: a loop keeps writing for as long as it runs and blows
/// past any small bound, while the background settles well inside this one.
const LOOP_LIMIT: u64 = 15;

/// Wait for `rel` to appear on `host`, with a deadline rather than a sleep.
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
                path: PathBuf::from(rel),
                content: content.into(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_arrives_without_waiting_for_the_next_dial() {
    let net = LoopbackNetwork::new();
    let (a, b) = common::synced_pair(&net).await;
    common::settle(&[&a, &b]).await;

    let started = std::time::Instant::now();
    write(&a, "quick.md", "now").await;
    assert_eq!(await_file(&b, "quick.md").await, "now");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "a live session should deliver in well under the dial interval, took {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_propagates_through_a_host_in_the_middle() {
    // A and S are connected; S and B are connected; A and B are not. This is the topology a
    // self-hosted server exists for.
    let net = LoopbackNetwork::partitioned(&[
        (common::NODE_A, common::NODE_S),
        (common::NODE_S, common::NODE_B),
    ]);
    let (a, s, b) = common::synced_triple(&net).await;
    common::settle(&[&a, &s, &b]).await;

    write(&a, "relayed.md", "from a").await;
    assert_eq!(await_file(&b, "relayed.md").await, "from a");
    let _ = s;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_is_not_forwarded_back_to_the_peer_it_came_from() {
    let net = LoopbackNetwork::new();
    let (a, b) = common::synced_pair(&net).await;
    common::settle(&[&a, &b]).await;

    // A quiet baseline first, so the count below covers this one write and not the
    // background the bridge was already making.
    let before = common::quiet(&[&b], QUIET, QUIET_DEADLINE).await[0];
    write(&a, "one-way.md", "x").await;
    await_file(&b, "one-way.md").await;
    // Wait for the write's own traffic to stop rather than assuming a second covers it.
    common::quiet(&[&b], QUIET, QUIET_DEADLINE).await;

    let grown = common::frames_sent(&b) - before;
    assert!(
        grown < LOOP_LIMIT,
        "B sent {grown} frames for one incoming file; it is echoing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn three_connected_hosts_do_not_loop_an_edit_between_them() {
    let net = LoopbackNetwork::new(); // fully connected
    let (a, s, b) = common::synced_triple(&net).await;
    common::settle(&[&a, &s, &b]).await;

    write(&a, "triangle.md", "x").await;
    await_file(&b, "triangle.md").await;
    await_file(&s, "triangle.md").await;

    // Wait for any loop to *die down* rather than assuming a fixed sleep covers it:
    // `quiet` returns only once A and B have both held still for QUIET, so a straggler on a
    // loaded runner delays the baseline instead of being counted as a loop. A host that
    // never stops talking never reaches quiet, and `quiet` fails on its deadline.
    let before = common::quiet(&[&a, &b], QUIET, QUIET_DEADLINE).await;

    // Then watch a longer window for a loop to *start*: the storm this test rules out keeps
    // writing the whole time, while the background the servers always make stays small.
    tokio::time::sleep(OBSERVE).await;
    let grown = [
        common::frames_sent(&a) - before[0],
        common::frames_sent(&b) - before[1],
    ];
    assert!(
        grown[0] < LOOP_LIMIT && grown[1] < LOOP_LIMIT,
        "A or B is still sending after the edit was propagated: A +{}, B +{} over {OBSERVE:?}",
        grown[0],
        grown[1]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_goes_away_is_redialled() {
    let net = LoopbackNetwork::new();
    let (a, b) = common::synced_pair(&net).await;
    common::settle(&[&a, &b]).await;

    b.drop_connections().await;
    write(&a, "after-drop.md", "x").await;

    assert_eq!(
        await_file(&b, "after-drop.md").await,
        "x",
        "a dropped connection must be redialled, not mourned"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_peer_does_not_stall_the_others() {
    let net = LoopbackNetwork::partitioned(&[(common::NODE_A, common::NODE_B)]);
    let (a, s, b) = common::synced_triple(&net).await;
    // S is listed in the workgroup but reachable by nobody.
    common::settle(&[&a, &b]).await;

    write(&a, "despite-s.md", "x").await;
    assert_eq!(await_file(&b, "despite-s.md").await, "x");
    let _ = s;
}

/// Wait for `rel` on `host` to hold `want`, with a deadline rather than a sleep.
async fn await_contents(host: &common::Host, rel: &str, want: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if std::fs::read_to_string(host.ws.join(rel)).is_ok_and(|text| text == want) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{rel} never became {want:?} on {}",
            host.ws.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Wait for `rel` on `host` to be gone, with a deadline rather than a sleep.
async fn await_absent(host: &common::Host, rel: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if std::fs::symlink_metadata(host.ws.join(rel)).is_err() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{rel} never disappeared from {}",
            host.ws.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_after_a_remote_edit_propagates_over_a_live_session() {
    let net = LoopbackNetwork::new();
    let (a, b) = common::synced_pair(&net).await;
    common::settle(&[&a, &b]).await;

    // a creates the file through its app server; b receives it over the live session.
    write(&a, "note.md", "one").await;
    assert_eq!(await_file(&b, "note.md").await, "one");

    // b edits it outside the server, and the live session carries it back to a.
    std::fs::write(b.ws.join("note.md"), "two").unwrap();
    await_contents(&a, "note.md", "two").await;

    // a deletes it; the delete must reach b.
    std::fs::remove_file(a.ws.join("note.md")).unwrap();
    await_absent(&b, "note.md").await;
}
