//! A real `AppServer` with sync turned on: the watcher, the replica and the IPC methods,
//! wired together the way an application wires them.
//!
//! The unit tests in `sync::watch` cover the watcher on its own, and `sync::methods` covers
//! the three methods over an in-process connection. What neither covers is the join: that
//! `AppServer::sync` really composes them into the router it serves, that an edit made
//! outside the server is noticed through the watcher, and that a write through the server
//! takes the exact path without waiting for a debounce.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sapphire_backend::protocol as proto;
use sapphire_framework_server::sync::testing::StubBridge;
use sapphire_framework_server::{AppServer, SyncRuntime};
use sapphire_ipc::{ClientInfo, Endpoint, ManagedBy, connect_or_absent};
use sapphire_workspace::{AppContext, AppKind};

static CTX: AppContext = AppContext::new("sapphire-syncwiring");

const DIR_VARS: [&str; 3] = [
    "SAPPHIRE_SYNCWIRING_CACHE_DIR",
    "SAPPHIRE_SYNCWIRING_DATA_DIR",
    "SAPPHIRE_SYNCWIRING_CONFIG_DIR",
];

/// Point the context's directories at this test's scratch tree, and put them back on drop.
struct EnvGuard {
    previous: [Option<std::ffi::OsString>; 3],
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: `self._lock` still serialises the environment; it is dropped only after
        // this method returns.
        for (name, previous) in DIR_VARS.iter().zip(self.previous.iter_mut()) {
            match previous.take() {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

/// Point the context's directories at `tmp` while holding the environment lock.
fn point_at(tmp: &std::path::Path) -> EnvGuard {
    // The static `CTX` is first-writer-wins, so every test in this binary must serialise on
    // the environment even though they do not share a root.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let previous = DIR_VARS.map(std::env::var_os);
    // SAFETY (via the lock above): serialised against every other use in this binary.
    for (name, dir) in DIR_VARS
        .iter()
        .zip(["cache", "data", "config"].map(|cat| tmp.join(cat)))
    {
        unsafe { std::env::set_var(name, dir) };
    }
    CTX.init(AppKind::Server);
    EnvGuard {
        previous,
        _lock: lock,
    }
}

/// Wait until the server listens on `endpoint`.
async fn wait_until_listening(endpoint: &Endpoint) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !sapphire_ipc::probe(endpoint).await.unwrap() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server never started listening"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait for `check` to hold, polling a condition rather than sleeping a fixed time.
async fn until(mut check: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn client_info() -> ClientInfo {
    ClientInfo {
        kind: "test".into(),
        version: "0.0.0".into(),
        api: sapphire_backend::protocol::API_VERSION,
        pid: std::process::id(),
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    _env: EnvGuard,
    endpoint: Endpoint,
    root: PathBuf,
    stub: StubBridge,
    client: sapphire_ipc::Client,
    server: tokio::task::JoinHandle<sapphire_framework_server::Result<()>>,
}

impl Fixture {
    async fn start() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let env = point_at(tmp.path());

        let root = tmp.path().join("ws");
        std::fs::create_dir_all(root.join(".sapphire-syncwiring")).unwrap();
        let root = root.canonicalize().unwrap();

        let (stub, bridge) = StubBridge::start().await;
        let runtime = Arc::new(SyncRuntime::new(
            &CTX,
            bridge,
            "/bin/true".into(),
            ManagedBy::Service,
        ));

        let endpoint = Endpoint::in_dir("sapphire-syncwiring", tmp.path().to_path_buf());
        let server = tokio::spawn(
            AppServer::new(&CTX, "0.0.0")
                .endpoint(endpoint.clone())
                .sync(runtime)
                .run(),
        );
        wait_until_listening(&endpoint).await;

        let (client, _) = connect_or_absent(&endpoint, "sapphire-syncwiring", client_info())
            .await
            .unwrap()
            .expect("the server is listening");

        Fixture {
            _tmp: tmp,
            _env: env,
            endpoint,
            root,
            stub,
            client,
            server,
        }
    }

    async fn enable(&self) -> proto::SyncEnableResult {
        self.client
            .call(
                proto::SYNC_ENABLE,
                proto::WsParams {
                    ws: self.root.clone(),
                },
            )
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_synced_server_answers_sync_enable_and_registers_with_the_bridge() {
    let f = Fixture::start().await;

    let result = f.enable().await;
    assert_eq!(
        f.stub.last_workspaces(),
        vec![result.workspace_id],
        "a real server's sync.enable must reach the bridge"
    );

    let status: proto::SyncStatusResult = f
        .client
        .call(proto::SYNC_STATUS, proto::WsParams { ws: f.root.clone() })
        .await
        .unwrap();
    assert!(status.enabled);
    assert_eq!(status.workspace_id, Some(result.workspace_id));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_sync_does_not_answer_sync_enable() {
    // The counter-case that gives the wiring test its meaning: the same `AppServer` with no
    // runtime has no sync at all, rather than a sync that silently does nothing.
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(tmp.path());

    let endpoint = Endpoint::in_dir("sapphire-syncwiring", tmp.path().to_path_buf());
    let server = tokio::spawn(
        AppServer::new(&CTX, "0.0.0")
            .endpoint(endpoint.clone())
            .run(),
    );
    wait_until_listening(&endpoint).await;

    let (client, _) = connect_or_absent(&endpoint, "sapphire-syncwiring", client_info())
        .await
        .unwrap()
        .expect("the server is listening");

    let err = client
        .call::<_, proto::SyncEnableResult>(
            proto::SYNC_ENABLE,
            proto::WsParams {
                ws: tmp.path().join("ws"),
            },
        )
        .await
        .expect_err("without a runtime there is no sync.enable");
    assert!(!err.to_string().is_empty());
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_made_outside_the_server_is_noticed_by_the_watcher() {
    let f = Fixture::start().await;
    f.enable().await;
    let before = f.stub.peers_queries();

    // As if the user had opened an editor. Nothing asks the bridge until the watcher has
    // fired and the runtime has scanned and dialled, so `peers` growing is the trace.
    std::fs::write(f.root.join("by-hand.md"), "typed directly").unwrap();
    until(
        || f.stub.peers_queries() > before,
        "the watcher to notice an edit the server did not make",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_through_the_server_is_scanned_without_the_watcher() {
    let f = Fixture::start().await;
    f.enable().await;

    let _: proto::Ack = f
        .client
        .call(
            proto::WRITE_FILE,
            proto::ContentParams {
                ws: f.root.clone(),
                path: PathBuf::from("via-ipc.md"),
                content: "through the server".into(),
            },
        )
        .await
        .unwrap();

    // The server's own write takes the exact path in `handlers.rs`, so the replica now
    // holds the file. Reading it back through the same server is the observable evidence
    // that the write and the scan both happened.
    let read: proto::ReadResult = f
        .client
        .call(
            proto::READ_FILE,
            proto::PathParams {
                ws: f.root.clone(),
                path: PathBuf::from("via-ipc.md"),
            },
        )
        .await
        .unwrap();
    assert_eq!(read.content, "through the server");
    let _ = &f.endpoint;
}
