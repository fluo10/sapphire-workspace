//! The server's one workspace end to end (#215): `workspace.init` selects what it creates,
//! `workspace.current` reports it, sync is switched on and off for it, and a restarting
//! server comes back to it — and to its sync — from `workspace.toml`, or from the list an
//! older server kept.
//!
//! Each test owns its `AppContext` (the directories are first-writer-wins per context), so
//! the file one test writes is never the one another reads.

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

async fn current(c: &Client) -> Option<proto::CurrentWorkspace> {
    c.call::<_, proto::WorkspaceCurrentResult>(proto::WORKSPACE_CURRENT, serde_json::json!({}))
        .await
        .unwrap()
        .workspace
}

async fn init(c: &Client, dir: PathBuf) -> proto::WorkspaceInitResult {
    c.call(proto::WORKSPACE_INIT, proto::WorkspaceInitParams { dir })
        .await
        .unwrap()
}

async fn stop(ctx: &'static AppContext, endpoint: &Endpoint, task: tokio::task::JoinHandle<()>) {
    let c = client(ctx, endpoint).await;
    let _: serde_json::Value = c
        .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
        .await
        .unwrap();
    task.await.unwrap();
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn init_selects_and_sync_switches_on_and_off_for_the_current_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_A, VARS_A, tmp.path());
    let endpoint = Endpoint::in_dir("sapphire-hostreg-a", tmp.path().join("run"));
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let (stub, task) = start(&CTX_A, &endpoint).await;
    let c = client(&CTX_A, &endpoint).await;
    assert!(current(&c).await.is_none(), "no workspace before the first");

    let root = tmp.path().join("notes");
    std::fs::create_dir_all(&root).unwrap();
    let made = init(&c, root.clone()).await;
    let ws = current(&c).await.unwrap();
    assert_eq!(ws.root, made.root);
    assert!(ws.reachable);
    assert!(!ws.sync.enabled);
    assert_eq!(ws.workspace_id, None, "describing never mints a sync id");

    let enabled: proto::SyncEnableResult = c
        .call(proto::SYNC_ENABLE, serde_json::json!({}))
        .await
        .unwrap();
    let ws = current(&c).await.unwrap();
    assert!(ws.sync.enabled);
    assert_eq!(ws.workspace_id, Some(enabled.workspace_id));

    let _: proto::Ack = c
        .call(proto::SYNC_DISABLE, serde_json::json!({}))
        .await
        .unwrap();
    let ws = current(&c).await.unwrap();
    assert!(!ws.sync.enabled);
    assert_eq!(
        ws.workspace_id,
        Some(enabled.workspace_id),
        "a disabled workspace keeps its id"
    );
    drop(stub);
    drop(c);
    stop(&CTX_A, &endpoint, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_comes_back_to_the_workspace_and_its_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_B, VARS_B, tmp.path());
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let endpoint = Endpoint::in_dir("sapphire-hostreg-b", tmp.path().join("run"));

    let (_stub, task) = start(&CTX_B, &endpoint).await;
    let c = client(&CTX_B, &endpoint).await;
    let root = tmp.path().join("kept");
    std::fs::create_dir_all(&root).unwrap();
    let made = init(&c, root).await;
    let e: proto::SyncEnableResult = c
        .call(proto::SYNC_ENABLE, serde_json::json!({}))
        .await
        .unwrap();
    drop(c);
    stop(&CTX_B, &endpoint, task).await;

    let (stub, task) = start(&CTX_B, &endpoint).await;
    until("the workspace's sync to come back", || {
        stub.last_workspaces().contains(&e.workspace_id)
    })
    .await;
    let c = client(&CTX_B, &endpoint).await;
    assert_eq!(current(&c).await.unwrap().root, made.root);
    drop(c);
    stop(&CTX_B, &endpoint, task).await;
}

static CTX_D: AppContext = AppContext::new("sapphire-hostreg-d");
const VARS_D: [&str; 3] = [
    "SAPPHIRE_HOSTREG_D_CACHE_DIR",
    "SAPPHIRE_HOSTREG_D_DATA_DIR",
    "SAPPHIRE_HOSTREG_D_CONFIG_DIR",
];

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_gone_while_stopped_stays_selected_and_unreachable() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_D, VARS_D, tmp.path());
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let endpoint = Endpoint::in_dir("sapphire-hostreg-d", tmp.path().join("run"));
    let (_stub, task) = start(&CTX_D, &endpoint).await;
    let c = client(&CTX_D, &endpoint).await;
    let root = tmp.path().join("vanishing");
    std::fs::create_dir_all(&root).unwrap();
    let made = init(&c, root.clone()).await;
    drop(c);
    stop(&CTX_D, &endpoint, task).await;
    std::fs::remove_dir_all(&root).unwrap();

    let (_stub, task) = start(&CTX_D, &endpoint).await;
    let c = client(&CTX_D, &endpoint).await;
    let ws = current(&c).await.expect("still the selected workspace");
    assert_eq!(ws.root, made.root);
    assert!(!ws.reachable);
    let err = c
        .call::<_, proto::ReadResult>(
            proto::READ_FILE,
            proto::PathParams {
                path: PathBuf::from("a.md"),
            },
        )
        .await;
    assert!(
        err.is_err(),
        "nothing to read from a workspace that is gone"
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
async fn the_cli_renders_the_current_workspace_or_says_there_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_C, VARS_C, tmp.path());
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let endpoint = Endpoint::in_dir("sapphire-hostreg-c", tmp.path().join("run"));
    let (_stub, task) = start(&CTX_C, &endpoint).await;
    let c = client(&CTX_C, &endpoint).await;

    let mut out = String::new();
    let code = sapphire_framework_server::render_workspace(&c, &mut out)
        .await
        .unwrap();
    assert_eq!(code, 1);
    assert!(out.contains("no workspace yet"), "{out}");

    let root = tmp.path().join("notes");
    std::fs::create_dir_all(&root).unwrap();
    init(&c, root).await;
    let mut out = String::new();
    let code = sapphire_framework_server::render_workspace(&c, &mut out)
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert!(out.contains("notes"), "{out}");
    assert!(out.contains("not synced"), "{out}");
    drop(c);
    stop(&CTX_C, &endpoint, task).await;
}

static CTX_E: AppContext = AppContext::new("sapphire-hostreg-e");
const VARS_E: [&str; 3] = [
    "SAPPHIRE_HOSTREG_E_CACHE_DIR",
    "SAPPHIRE_HOSTREG_E_DATA_DIR",
    "SAPPHIRE_HOSTREG_E_CONFIG_DIR",
];

#[tokio::test(flavor = "multi_thread")]
async fn an_older_servers_list_becomes_the_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = point_at(&CTX_E, VARS_E, tmp.path());
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let endpoint = Endpoint::in_dir("sapphire-hostreg-e", tmp.path().join("run"));
    let synced = tmp.path().join("synced");
    std::fs::create_dir_all(synced.join(".sapphire-hostreg-e")).unwrap();
    let synced = synced.canonicalize().unwrap();
    // The config directory is `<category root>/<app name>`.
    let config = tmp.path().join("config").join(CTX_E.app_name);
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("workspaces.toml"),
        format!(
            "[workspace.other]\nroot = \"/nowhere\"\n\n[workspace.synced]\nroot = {:?}\nsynced = true\n",
            synced.display().to_string()
        ),
    )
    .unwrap();

    let (stub, task) = start(&CTX_E, &endpoint).await;
    let c = client(&CTX_E, &endpoint).await;
    assert_eq!(current(&c).await.unwrap().root, synced);
    until("the migrated workspace to sync", || {
        !stub.last_workspaces().is_empty()
    })
    .await;
    drop(c);
    stop(&CTX_E, &endpoint, task).await;
}
