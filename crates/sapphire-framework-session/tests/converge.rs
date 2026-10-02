//! Two replicas in one process, connected by a duplex, must converge.

use std::path::Path;
use std::sync::Arc;

use sapphire_framework_session::run_session;
use sapphire_sync::{Replica, ReplicaConfig, SystemClock};
use tokio::sync::Mutex;

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    // Behind a `Mutex`, not owned directly: `run_session` only locks it from after the
    // peer's `Hello` through to the end of the exchange (issue #163), so passing it the
    // mutex itself — rather than pre-locking around the whole call, as every fixture here
    // once did — is what the fix is for.
    replica: Mutex<Replica>,
}

fn replica(name: &str, device: grain_id::GrainId) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(name);
    std::fs::create_dir_all(root.join(".test-app")).unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let config = ReplicaConfig::new("test-app", root.clone(), device, &state);
    let replica = Replica::open(config, Arc::new(SystemClock)).unwrap();
    Fixture {
        _tmp: tmp,
        root,
        replica: Mutex::new(replica),
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Scan `fixture`'s replica — a single-locker convenience for a test with nothing else
/// touching it concurrently.
async fn scan(fixture: &Fixture) {
    fixture.replica.lock().await.scan().unwrap();
}

/// Run one session between `a` and `b` over an in-process duplex.
async fn sync(a: &Fixture, b: &Fixture, ws: grain_id::GrainId) {
    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        run_session(left, &a.replica, ws),
        run_session(right, &b.replica, ws)
    );
    x.unwrap();
    y.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_written_on_one_side_appears_on_the_other() {
    let ws = grain_id::GrainId::random();
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    write(&a.root, "notes/hello.md", "# hello");
    scan(&a).await;

    sync(&a, &b, ws).await;

    assert_eq!(
        std::fs::read_to_string(b.root.join("notes/hello.md")).unwrap(),
        "# hello"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_larger_than_the_inline_limit_is_fetched_by_hash() {
    let ws = grain_id::GrainId::random();
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    let big = "x".repeat(300_000);
    write(&a.root, "big.md", &big);
    scan(&a).await;

    sync(&a, &b, ws).await;

    assert_eq!(
        std::fs::read_to_string(b.root.join("big.md"))
            .unwrap()
            .len(),
        300_000
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_session_sends_nothing_new() {
    let ws = grain_id::GrainId::random();
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    write(&a.root, "a.md", "one");
    scan(&a).await;
    sync(&a, &b, ws).await;

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, _) = tokio::join!(
        run_session(left, &a.replica, ws),
        run_session(right, &b.replica, ws)
    );
    assert_eq!(
        x.unwrap().sent,
        0,
        "nothing new should be sent the second time"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_on_both_sides_both_arrive() {
    let ws = grain_id::GrainId::random();
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    write(&a.root, "from-a.md", "a");
    write(&b.root, "from-b.md", "b");
    scan(&a).await;
    scan(&b).await;

    sync(&a, &b, ws).await;

    assert!(b.root.join("from-a.md").exists());
    assert!(a.root.join("from-b.md").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_propagates() {
    let ws = grain_id::GrainId::random();
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    write(&a.root, "doomed.md", "x");
    scan(&a).await;
    sync(&a, &b, ws).await;
    assert!(b.root.join("doomed.md").exists());

    std::fs::remove_file(a.root.join("doomed.md")).unwrap();
    scan(&a).await;
    sync(&a, &b, ws).await;

    assert!(!b.root.join("doomed.md").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_edits_leave_a_conflict_copy_rather_than_losing_one() {
    let ws = grain_id::GrainId::random();
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    write(&a.root, "shared.md", "seed");
    scan(&a).await;
    sync(&a, &b, ws).await;

    // Both edit without seeing the other.
    write(&a.root, "shared.md", "from a");
    write(&b.root, "shared.md", "from b");
    scan(&a).await;
    scan(&b).await;

    sync(&a, &b, ws).await;

    let names: Vec<String> = std::fs::read_dir(&b.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.contains(".conflict-")),
        "the losing edit must survive as a conflict copy; saw {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_for_a_different_workspace_is_refused() {
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    let (left, right) = tokio::io::duplex(64 * 1024);
    let (x, y) = tokio::join!(
        run_session(left, &a.replica, grain_id::GrainId::random()),
        run_session(right, &b.replica, grain_id::GrainId::random())
    );
    assert!(
        x.is_err() || y.is_err(),
        "mismatched workspaces must not exchange state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_session_does_not_advance_the_version_vector() {
    let ws = grain_id::GrainId::random();
    let a = replica("a", grain_id::GrainId::random());
    let b = replica("b", grain_id::GrainId::random());

    write(&a.root, "a.md", "one");
    scan(&a).await;

    // B's side is dropped as soon as it has said Hello, so A never sees Done.
    let (left, right) = tokio::io::duplex(64 * 1024);
    let before = b.replica.lock().await.vv().clone();
    let cut = tokio::spawn(async move {
        let mut right = right;
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 16];
        let _ = right.read(&mut buf).await;
        drop(right);
    });
    let _ = run_session(left, &a.replica, ws).await;
    cut.await.unwrap();

    assert_eq!(
        b.replica.lock().await.vv(),
        &before,
        "an interrupted session must leave the version vector untouched"
    );
}
