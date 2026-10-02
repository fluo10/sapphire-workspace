//! The replica must not be locked while an exchange merely waits for a peer's `Hello`
//! (issue #163).

use std::sync::Arc;
use std::time::Duration;

use sapphire_framework_session::run_session;
use sapphire_sync::{Replica, ReplicaConfig, SystemClock};
use tokio::sync::Mutex;

fn replica() -> Replica {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join(".test-app")).unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let config = ReplicaConfig::new("test-app", root, grain_id::GrainId::random(), &state);
    // Leaked: the test only needs the replica store to outlive the test body, and a
    // `TempDir` held by value would need threading through every closure below for no
    // benefit — this is a short-lived test process.
    std::mem::forget(tmp);
    Replica::open(config, Arc::new(SystemClock)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replica_is_not_locked_while_an_exchange_waits_for_the_peers_hello() {
    let shared = Arc::new(Mutex::new(replica()));
    let ws = grain_id::GrainId::random();

    // The peer's half is held open and never written to: `run_session` will sit in its
    // `Hello` wait for the full `HELLO_TIMEOUT`, exactly like a bridge that parked this
    // stream for an owner that has not claimed it yet.
    let (mine, theirs) = tokio::io::duplex(64 * 1024);
    let waiting = Arc::clone(&shared);
    let session = tokio::spawn(async move {
        let _ = run_session(mine, &waiting, ws).await;
    });

    // While that session is stuck waiting for a `Hello` that is never coming, the same
    // replica must still be free for anything else — a scan, another session — to use.
    // Blocked on this lock is exactly the failure mode #163 describes.
    let scanned =
        tokio::time::timeout(Duration::from_secs(2), async { shared.lock().await.scan() }).await;
    assert!(
        scanned.is_ok(),
        "the replica must not be locked merely because a session is waiting for Hello"
    );

    drop(theirs);
    session.abort();
}
