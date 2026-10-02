//! A slow inbound session for one workspace must not stall every other peer stream
//! (issue #164).

mod common;

use std::time::Duration;

use sapphire_framework_bridge::{BridgeDir, LoopbackNetwork, PeerTransport, Workgroup};

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_workgroup_session_does_not_block_an_ordinary_stream_behind_it() {
    let net = LoopbackNetwork::new();
    let a = common::start(&net, common::NODE_A, "host-a").await;

    // A second identity, authorized in A's workgroup but never actually run as a bridge:
    // all it needs to do is open raw streams toward A.
    let tmp = tempfile::tempdir().unwrap();
    let dir_c = BridgeDir::at(tmp.path().join("bridge")).unwrap();
    let wg_c = Workgroup::create(&dir_c, "test", "host-c", common::NODE_C).unwrap();
    common::introduce(&dir_c, wg_c.id, "host-c", &a.dir, a.workgroup_id);
    let from_c = net.transport(common::NODE_C);

    // An ordinary app workspace registered on A, with its owner's announcement channel
    // subscribed before anything is sent.
    let client_a = common::connect(&a).await;
    let ws = grain_id::GrainId::random();
    client_a.register(a.registration(ws)).await.unwrap();
    let mut incoming = client_a.incoming();

    // First: a stream for A's own workgroup workspace, held open and never written to.
    // `WorkgroupReplica::session` waits for a `Hello` that is never coming — exactly the
    // state a bridge the dial loop is racing (#159) or a peer that simply stalled leaves
    // behind.
    let wg_stream = from_c
        .open(common::NODE_A, a.workgroup_id)
        .await
        .expect("opening the workgroup stream");

    // Second, right after: a stream for the ordinary workspace. The accept loop must reach
    // this and announce it promptly — not wait behind the first stream's session.
    let ws_stream = from_c
        .open(common::NODE_A, ws)
        .await
        .expect("opening the ordinary-workspace stream");

    let announced = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await;
    assert!(
        announced.is_ok(),
        "a stream for an ordinary workspace must not wait behind an unrelated, stalled \
         workgroup session"
    );
    assert_eq!(announced.unwrap().unwrap().workspace_id, ws);

    drop(wg_stream);
    drop(ws_stream);
}
