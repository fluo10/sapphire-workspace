//! A session that keeps going after the initial exchange.

use std::sync::Arc;

use sapphire_framework_session::open_live_session;
use sapphire_sync::{Replica, ReplicaConfig, SystemClock};
use tokio::sync::Mutex;

struct Side {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    replica: Arc<Mutex<Replica>>,
}

fn side(name: &str) -> Side {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(name);
    std::fs::create_dir_all(root.join(".test-app")).unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let config = ReplicaConfig::new(
        "test-app",
        root.clone(),
        grain_id::GrainId::random(),
        &state,
    );
    let replica = Replica::open(config, Arc::new(SystemClock)).unwrap();
    Side {
        _tmp: tmp,
        root,
        replica: Arc::new(Mutex::new(replica)),
    }
}

async fn scan(side: &Side) -> Vec<sapphire_sync::PathUpdate> {
    let mut replica = side.replica.lock().await;
    let outcome = replica.scan().unwrap();
    match outcome {
        sapphire_sync::ScanOutcome::Scanned(report) => report
            .recorded
            .into_iter()
            .map(|entry| sapphire_sync::PathUpdate {
                path: entry.path.clone(),
                versions: vec![entry],
                seen: replica.vv().clone(),
            })
            .collect(),
        sapphire_sync::ScanOutcome::Paused(_) => vec![],
    }
}

/// Wait until `rel` exists on `side`, or fail.
async fn await_file(side: &Side, rel: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(side.root.join(rel)) {
            return text;
        }
        assert!(std::time::Instant::now() < deadline, "{rel} never arrived");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_initial_exchange_still_happens() {
    let ws = grain_id::GrainId::random();
    let a = side("a");
    let b = side("b");
    std::fs::write(a.root.join("first.md"), "before the session").unwrap();
    scan(&a).await;

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        open_live_session(left, Arc::clone(&a.replica), ws),
        open_live_session(right, Arc::clone(&b.replica), ws)
    );
    let (outcome_a, _live_a) = x.unwrap();
    let (_outcome_b, _live_b) = y.unwrap();

    assert_eq!(outcome_a.sent, 1);
    assert_eq!(await_file(&b, "first.md").await, "before the session");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_after_the_exchange_arrives_without_a_new_session() {
    let ws = grain_id::GrainId::random();
    let a = side("a");
    let b = side("b");

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        open_live_session(left, Arc::clone(&a.replica), ws),
        open_live_session(right, Arc::clone(&b.replica), ws)
    );
    let (_, live_a) = x.unwrap();
    let (_, _live_b) = y.unwrap();

    std::fs::write(a.root.join("later.md"), "after the session started").unwrap();
    let updates = scan(&a).await;
    live_a.push(updates).await.unwrap();

    assert_eq!(
        await_file(&b, "later.md").await,
        "after the session started"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_sees_what_arrived() {
    let ws = grain_id::GrainId::random();
    let a = side("a");
    let b = side("b");

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        open_live_session(left, Arc::clone(&a.replica), ws),
        open_live_session(right, Arc::clone(&b.replica), ws)
    );
    let (_, live_a) = x.unwrap();
    let (_, live_b) = y.unwrap();

    let mut received = live_b.updates();
    std::fs::write(a.root.join("watched.md"), "x").unwrap();
    live_a.push(scan(&a).await).await.unwrap();

    let updates = tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
        .await
        .expect("an update within ten seconds")
        .unwrap();
    assert!(
        updates.iter().any(|u| u.path.ends_with("watched.md")),
        "{updates:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_peers_version_vector_advances_as_pushes_are_taken() {
    let ws = grain_id::GrainId::random();
    let a = side("a");
    let b = side("b");

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        open_live_session(left, Arc::clone(&a.replica), ws),
        open_live_session(right, Arc::clone(&b.replica), ws)
    );
    let (_, live_a) = x.unwrap();
    let (_, live_b) = y.unwrap();

    let before = live_a.peer_vv();
    std::fs::write(b.root.join("from-b.md").clone(), "x").unwrap();
    let updates = scan(&b).await;
    live_b.push(updates).await.unwrap();
    await_file(&a, "from-b.md").await;

    assert_ne!(
        live_a.peer_vv(),
        before,
        "the peer's version vector must advance, or every entry is forwarded for ever"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_one_side_is_visible_on_the_other() {
    let ws = grain_id::GrainId::random();
    let a = side("a");
    let b = side("b");

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        open_live_session(left, Arc::clone(&a.replica), ws),
        open_live_session(right, Arc::clone(&b.replica), ws)
    );
    let (_, live_a) = x.unwrap();
    let (_, live_b) = y.unwrap();

    live_a.close();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while live_b.is_open() {
        assert!(
            std::time::Instant::now() < deadline,
            "the close was never noticed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pushing_on_a_closed_session_fails_rather_than_hanging() {
    let ws = grain_id::GrainId::random();
    let a = side("a");
    let b = side("b");

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        open_live_session(left, Arc::clone(&a.replica), ws),
        open_live_session(right, Arc::clone(&b.replica), ws)
    );
    let (_, live_a) = x.unwrap();
    let (_, live_b) = y.unwrap();
    live_b.close();

    // Give the close a moment to reach A, then push.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    std::fs::write(a.root.join("into-the-void.md"), "x").unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        live_a.push(scan(&a).await),
    )
    .await;
    assert!(result.is_ok(), "push must not hang on a closed session");
}
#[tokio::test(flavor = "multi_thread")]
async fn a_push_larger_than_the_inline_limit_is_fetched_by_hash() {
    let ws = grain_id::GrainId::random();
    let a = side("a");
    let b = side("b");

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        open_live_session(left, Arc::clone(&a.replica), ws),
        open_live_session(right, Arc::clone(&b.replica), ws)
    );
    let (_, live_a) = x.unwrap();
    let (_, live_b) = y.unwrap();

    // A `Live` message carries no content, so this is the path that asks for it by hash
    // and only announces the batch once the bytes are on disk.
    let mut received = live_b.updates();
    let big = "x".repeat(300_000);
    std::fs::write(a.root.join("big.md"), &big).unwrap();
    live_a.push(scan(&a).await).await.unwrap();

    let updates = tokio::time::timeout(std::time::Duration::from_secs(20), received.recv())
        .await
        .expect("an update within twenty seconds")
        .unwrap();
    assert!(
        updates.iter().any(|u| u.path.ends_with("big.md")),
        "{updates:?}"
    );
    // And by the time it is announced the file is there, so a forwarder can serve it.
    assert_eq!(await_file(&b, "big.md").await.len(), 300_000);
}

#[tokio::test(start_paused = true)]
async fn an_exchange_gives_up_on_a_peer_that_never_says_hello() {
    // The far end took the stream and then said nothing: a peer whose bridge parked the
    // stream for an owner that never claimed it, or one that went away between the dial and
    // its `Hello`. Nothing closes the stream, so nothing ends the read on its own.
    //
    // The exchange holds the replica's lock, so waiting for ever pins this host's replica —
    // no scan, no commit and no other session for the rest of the process's life. The
    // session has to give up instead, and say so.
    let ws = grain_id::GrainId::random();
    let a = side("a");
    // Held, not dropped: a dropped half would end the read with end of file, which is the
    // case that already works. This one is a peer that is simply silent.
    let (mine, _theirs) = tokio::io::duplex(64 * 1024);

    let opened = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        open_live_session(mine, Arc::clone(&a.replica), ws),
    )
    .await
    .expect("a silent peer must not hold the exchange open for ever");

    let Err(err) = opened else {
        panic!("a session with a peer that never spoke must not be reported as open");
    };
    assert!(
        err.to_string().contains("Hello"),
        "the failure must name what was waited for, got: {err}"
    );
    assert!(
        a.replica.try_lock().is_ok(),
        "the replica's lock must be released when the exchange gives up"
    );
}
