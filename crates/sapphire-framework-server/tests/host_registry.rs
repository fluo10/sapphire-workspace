//! The host workspace registry end to end: `workspace.init` records a root, sync enable and
//! disable flip its `synced` flag, `workspace.list` reports it, `workspace.forget` drops it,
//! and a restarting server re-enables what was synced.
//!
//! Each test owns its `AppContext` (the directories are first-writer-wins per context), so
//! the registry file one test writes is never the one another reads.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sapphire_backend::protocol as proto;
use sapphire_framework_server::sync::testing::StubBridge;
use sapphire_framework_server::{AppServer, SyncRuntime};
use sapphire_ipc::{Client, ClientInfo, Endpoint, ManagedBy, connect_or_absent};
use sapphire_workspace::{AppContext, AppKind};

static CTX_A: AppContext = AppContext::new("sapphire-hostreg-a");
static CTX_B: AppContext = AppContext::new("sapphire-hostreg-b");

const VARS_A: [&str; 3] = [
    "SAPPHIRE_HOSTREG_A_CACHE_DIR",
    "SAPPHIRE_HOSTREG_A_DATA_DIR",
    "SAPPHIRE_HOSTREG_A_CONFIG_DIR",
];
const VARS_B: [&str; 3] = [
    "SAPPHIRE_HOSTREG_B_CACHE_DIR",
    "SAPPHIRE_HOSTREG_B_DATA_DIR",
    "SAPPHIRE_HOSTREG_B_CONFIG_DIR",
];

/// Point the context's directories at this test's scratch tree, and put them back on drop.
struct EnvGuard {
    vars: [&'static str; 3],
    previous: [Option<std::ffi::OsString>; 3],
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: `self._lock` still serialises the environment; it is dropped only after
        // this method returns.
        for (name, previous) in self.vars.iter().zip(self.previous.iter_mut()) {
            match previous.take() {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

/// Point `ctx`'s directories (named by `vars`) at `tmp` while holding the environment lock.
fn point_at(ctx: &'static AppContext, vars: [&'static str; 3], tmp: &std::path::Path) -> EnvGuard {
    // The contexts are first-writer-wins, so every test in this binary serialises on the
    // environment even though each has a context of its own.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let previous = vars.map(std::env::var_os);
    // SAFETY (via the lock above): serialised against every other use in this binary.
    for (name, dir) in vars
        .iter()
        .zip(["cache", "data", "config"].map(|cat| tmp.join(cat)))
    {
        unsafe { std::env::set_var(name, dir) };
    }
    ctx.init(AppKind::Server);
    EnvGuard {
        vars,
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

/// Start a sync-mounted server on `endpoint` against a fresh stub bridge.
async fn start(
    ctx: &'static AppContext,
    endpoint: &Endpoint,
) -> (StubBridge, tokio::task::JoinHandle<()>) {
    let (stub, bridge) = StubBridge::start().await;
    let runtime = Arc::new(SyncRuntime::new(
        ctx,
        bridge,
        PathBuf::from("/bin/true"),
        ManagedBy::Service,
    ));
    let server = AppServer::new(ctx, "0.0.0")
        .endpoint(endpoint.clone())
        .sync(runtime);
    let task = tokio::spawn(async move { server.run().await.unwrap() });
    wait_until_listening(endpoint).await;
    (stub, task)
}

async fn client(ctx: &'static AppContext, endpoint: &Endpoint) -> Client {
    let info = ClientInfo {
        kind: "test".into(),
        version: "0.0.0".into(),
        api: proto::API_VERSION,
        pid: std::process::id(),
    };
    connect_or_absent(endpoint, ctx.app_name, info)
        .await
        .unwrap()
        .unwrap()
        .0
}

async fn list(c: &Client) -> Vec<proto::WorkspaceListEntry> {
    c.call::<_, proto::WorkspaceListResult>(proto::WORKSPACE_LIST, serde_json::json!({}))
        .await
        .unwrap()
        .workspaces
}

async fn stop(ctx: &'static AppContext, endpoint: &Endpoint, task: tokio::task::JoinHandle<()>) {
    let c = client(ctx, endpoint).await;
    let _: serde_json::Value = c
        .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
        .await
        .unwrap();
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn init_enable_disable_and_forget_show_in_the_list() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_A, VARS_A, tmp.path());
    let endpoint = Endpoint::in_dir("sapphire-hostreg-a", tmp.path().join("run"));
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let (stub, task) = start(&CTX_A, &endpoint).await;
    let c = client(&CTX_A, &endpoint).await;

    let root = tmp.path().join("notes");
    std::fs::create_dir_all(&root).unwrap();
    let init: proto::WorkspaceInitResult = c
        .call(
            proto::WORKSPACE_INIT,
            proto::WorkspaceInitParams { dir: root.clone() },
        )
        .await
        .unwrap();
    let rows = list(&c).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].root, init.root);
    assert!(rows[0].reachable);
    assert!(!rows[0].sync.enabled);
    assert_eq!(rows[0].workspace_id, None, "listing never mints a sync id");

    let enabled: proto::SyncEnableResult = c
        .call(
            proto::SYNC_ENABLE,
            proto::WsParams {
                ws: init.root.clone(),
            },
        )
        .await
        .unwrap();
    let rows = list(&c).await;
    assert!(rows[0].sync.enabled);
    assert_eq!(rows[0].workspace_id, Some(enabled.workspace_id));

    let _: proto::Ack = c
        .call(
            proto::SYNC_DISABLE,
            proto::WsParams {
                ws: init.root.clone(),
            },
        )
        .await
        .unwrap();
    let rows = list(&c).await;
    assert!(!rows[0].sync.enabled);
    assert_eq!(
        rows[0].workspace_id,
        Some(enabled.workspace_id),
        "a disabled workspace keeps its id"
    );

    let _: proto::Ack = c
        .call(
            proto::WORKSPACE_FORGET,
            proto::WorkspaceForgetParams {
                id: rows[0].id.clone(),
            },
        )
        .await
        .unwrap();
    assert!(list(&c).await.is_empty());
    assert!(
        root.join(".sapphire-hostreg-a").is_dir(),
        "forgetting never touches files"
    );

    let err = c
        .call::<_, proto::Ack>(
            proto::WORKSPACE_FORGET,
            proto::WorkspaceForgetParams { id: "nope".into() },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nope"), "{err}");
    drop(stub);
    drop(c);
    stop(&CTX_A, &endpoint, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_restores_synced_rows_and_skips_missing_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_B, VARS_B, tmp.path());
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let endpoint = Endpoint::in_dir("sapphire-hostreg-b", tmp.path().join("run"));

    let (_stub, task) = start(&CTX_B, &endpoint).await;
    let c = client(&CTX_B, &endpoint).await;
    let mut ids = Vec::new();
    for name in ["gone", "kept"] {
        let root = tmp.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let init: proto::WorkspaceInitResult = c
            .call(
                proto::WORKSPACE_INIT,
                proto::WorkspaceInitParams { dir: root },
            )
            .await
            .unwrap();
        let e: proto::SyncEnableResult = c
            .call(proto::SYNC_ENABLE, proto::WsParams { ws: init.root })
            .await
            .unwrap();
        ids.push(e.workspace_id);
    }
    drop(c);
    stop(&CTX_B, &endpoint, task).await;
    std::fs::remove_dir_all(tmp.path().join("gone")).unwrap();

    let (stub, task) = start(&CTX_B, &endpoint).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !stub.last_workspaces().contains(&ids[1]) {
        assert!(
            std::time::Instant::now() < deadline,
            "the kept workspace was never restored"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!stub.last_workspaces().contains(&ids[0]));
    stop(&CTX_B, &endpoint, task).await;
}

static CTX_D: AppContext = AppContext::new("sapphire-hostreg-d");
const VARS_D: [&str; 3] = [
    "SAPPHIRE_HOSTREG_D_CACHE_DIR",
    "SAPPHIRE_HOSTREG_D_DATA_DIR",
    "SAPPHIRE_HOSTREG_D_CONFIG_DIR",
];

#[tokio::test(flavor = "multi_thread")]
async fn forget_tears_down_sync_even_when_the_root_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_D, VARS_D, tmp.path());
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let endpoint = Endpoint::in_dir("sapphire-hostreg-d", tmp.path().join("run"));
    let (stub, task) = start(&CTX_D, &endpoint).await;
    let c = client(&CTX_D, &endpoint).await;

    let root = tmp.path().join("vanishing");
    std::fs::create_dir_all(&root).unwrap();
    let init: proto::WorkspaceInitResult = c
        .call(
            proto::WORKSPACE_INIT,
            proto::WorkspaceInitParams { dir: root.clone() },
        )
        .await
        .unwrap();
    let enabled: proto::SyncEnableResult = c
        .call(proto::SYNC_ENABLE, proto::WsParams { ws: init.root })
        .await
        .unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let rows = list(&c).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].reachable);
    let _: proto::Ack = c
        .call(
            proto::WORKSPACE_FORGET,
            proto::WorkspaceForgetParams {
                id: rows[0].id.clone(),
            },
        )
        .await
        .unwrap();
    assert!(list(&c).await.is_empty());
    assert!(
        stub.seen
            .lock()
            .unwrap()
            .unregistrations
            .contains(&enabled.workspace_id),
        "forget left the workspace registered with the bridge"
    );
    drop(c);
    stop(&CTX_D, &endpoint, task).await;
}

static CTX_C: AppContext = AppContext::new("sapphire-hostreg-c");
const VARS_C: [&str; 3] = [
    "SAPPHIRE_HOSTREG_C_CACHE_DIR",
    "SAPPHIRE_HOSTREG_C_DATA_DIR",
    "SAPPHIRE_HOSTREG_C_CONFIG_DIR",
];

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_list_renders_the_servers_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_C, VARS_C, tmp.path());
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let endpoint = Endpoint::in_dir("sapphire-hostreg-c", tmp.path().join("run"));
    let (_stub, task) = start(&CTX_C, &endpoint).await;
    let c = client(&CTX_C, &endpoint).await;
    let root = tmp.path().join("notes");
    std::fs::create_dir_all(&root).unwrap();
    let _: proto::WorkspaceInitResult = c
        .call(
            proto::WORKSPACE_INIT,
            proto::WorkspaceInitParams { dir: root },
        )
        .await
        .unwrap();

    let mut out = String::new();
    let code = sapphire_framework_server::render_workspace_list(&c, &mut out)
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert!(out.contains("notes"), "{out}");
    assert!(out.contains("not synced"), "{out}");
    drop(c);
    stop(&CTX_C, &endpoint, task).await;
}
