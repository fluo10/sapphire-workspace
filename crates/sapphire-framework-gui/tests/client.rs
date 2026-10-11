use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sapphire_bridge_api::BridgeClient;
use sapphire_framework_bridge::{Bridge, BridgeDir, LoopbackNetwork, NetConfig};
use sapphire_framework_gui::client::*;
use sapphire_framework_server::{AppServer, SyncRuntime};
use sapphire_ipc::{Endpoint, ManagedBy};
use sapphire_workspace::{AppContext, AppKind};

static CTX: AppContext = AppContext::new("sapphire-guitest");
const APP: AppIdentity = AppIdentity {
    app_name: "sapphire-guitest",
    version: "0.0.0",
};
const NODE: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// Serialises the env vars `CTX` reads; one test at a time owns them.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    tmp: tempfile::TempDir,
    _env: std::sync::MutexGuard<'static, ()>,
    endpoints: Endpoints,
}

fn fixture() -> Fixture {
    let env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    for (var, cat) in [("CACHE", "cache"), ("DATA", "data"), ("CONFIG", "config")] {
        // SAFETY: serialised by `ENV` for every test in this binary.
        unsafe { std::env::set_var(format!("SAPPHIRE_GUITEST_{var}_DIR"), tmp.path().join(cat)) };
    }
    CTX.init(AppKind::Server);
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let endpoints = Endpoints {
        bridge: Endpoint::in_dir("bridge", run.clone()),
        app: Endpoint::in_dir(APP.app_name, run),
    };
    Fixture {
        tmp,
        _env: env,
        endpoints,
    }
}

fn config(f: &Fixture) -> ClientConfig {
    ClientConfig {
        app: APP,
        endpoints: f.endpoints.clone(),
        service_exes: ServiceExes {
            bridge: PathBuf::from("/nope/bridge"),
            app: PathBuf::from("/nope/app"),
        },
        refresh: Duration::from_millis(100),
        fetch_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(30),
        install_timeout: Duration::from_secs(300),
    }
}

/// A bridge running on a runtime of its own, so that stopping it ends every task it spawned.
///
/// Aborting one task would leave the per-connection handlers alive, and the client's cached
/// connection would keep answering — not what a bridge process exiting looks like.
struct BridgeProcess(Option<tokio::runtime::Runtime>);

impl BridgeProcess {
    fn stop(mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// A bridge with no workgroup, on the fixture's endpoints.
async fn start_bridge(f: &Fixture) -> BridgeProcess {
    start_bridge_as(
        f,
        &LoopbackNetwork::new(),
        NODE,
        "bridge",
        &f.endpoints.bridge,
    )
    .await
}

/// A bridge with no workgroup: node `node` on `net`, its directory `<tmp>/<name>`, its
/// control plane on `control`.
async fn start_bridge_as(
    f: &Fixture,
    net: &LoopbackNetwork,
    node: &str,
    name: &str,
    control: &Endpoint,
) -> BridgeProcess {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let guard = rt.enter();
    let dir = BridgeDir::at(f.tmp.path().join(name)).unwrap();
    let data = Endpoint::in_dir(format!("{name}-data"), control.dir.clone());
    let bridge = Bridge::new(dir, Arc::new(net.transport(node)), "0.0.0")
        .unwrap()
        .control_endpoint(control.clone())
        .data_endpoint(data);
    drop(guard);
    rt.spawn(async move {
        let _ = Arc::new(bridge).run_shared(NetConfig::default()).await;
    });
    wait(|| async { sapphire_ipc::probe(control).await.unwrap() }).await;
    BridgeProcess(Some(rt))
}

/// A sync-mounted app server talking to that bridge.
async fn start_server(f: &Fixture) -> tokio::task::JoinHandle<()> {
    let bridge = BridgeClient::connect_at(&f.endpoints.bridge, "test", "0.0.0")
        .await
        .unwrap()
        .unwrap();
    let runtime = Arc::new(SyncRuntime::new(
        &CTX,
        Arc::new(bridge),
        PathBuf::from("/bin/true"),
        ManagedBy::Service,
    ));
    let server = AppServer::new(&CTX, "0.0.0")
        .endpoint(f.endpoints.app.clone())
        .sync(runtime);
    let task = tokio::spawn(async move {
        let _ = server.run().await;
    });
    wait(|| async { sapphire_ipc::probe(&f.endpoints.app).await.unwrap() }).await;
    task
}

async fn wait<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "condition never held");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn outcome(client: &FrameworkClient, id: CommandId) -> Result<CommandOutput, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(o) = client.drain_outcomes().into_iter().find(|o| o.id == id) {
            return o.result;
        }
        assert!(Instant::now() < deadline, "no outcome for {id:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn spawn(f: &Fixture) -> FrameworkClient {
    FrameworkClient::spawn(
        &tokio::runtime::Handle::current(),
        config(f),
        Arc::new(|| {}),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_running_reads_absent_twice() {
    let f = fixture();
    let client = spawn(&f);
    wait(|| async { client.snapshot().fetched }).await;
    let s = (*client.snapshot()).clone();
    assert!(matches!(s.bridge, Conn::Absent));
    assert!(matches!(s.server, Conn::Absent));
    let id = client.send(Command::SyncEnable);
    let err = outcome(&client, id).await.unwrap_err();
    assert!(err.contains("not running"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_that_stops_reads_absent_and_one_that_returns_reads_up() {
    let f = fixture();
    let bridge = start_bridge(&f).await;
    let client = spawn(&f);
    wait(|| async { client.snapshot().bridge.up().is_some() }).await;
    bridge.stop();
    wait(|| async { matches!(client.snapshot().bridge, Conn::Absent | Conn::Error(_)) }).await;
    wait(|| async { matches!(client.snapshot().bridge, Conn::Absent) }).await;
    let _bridge = start_bridge(&f).await;
    wait(|| async { client.snapshot().bridge.up().is_some() }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn create_invite_and_retire_round_trip() {
    let f = fixture();
    let _bridge = start_bridge(&f).await;
    let client = spawn(&f);
    wait(|| async { client.snapshot().bridge.up().is_some() }).await;

    let id = client.send(Command::WorkgroupCreate {
        name: "home".into(),
        device_name: "desk".into(),
    });
    assert_eq!(outcome(&client, id).await, Ok(CommandOutput::Done));
    wait(|| async {
        client
            .snapshot()
            .bridge
            .up()
            .and_then(|b| b.status.workgroup.as_ref())
            .map(|w| w.name == "home")
            == Some(true)
    })
    .await;

    let id = client.send(Command::DeviceInvite {
        name: "laptop".into(),
        ttl_secs: Some(3600),
    });
    match outcome(&client, id).await {
        Ok(CommandOutput::Ticket(t)) => assert!(!t.is_empty()),
        other => panic!("expected a ticket, got {other:?}"),
    }

    let id = client.send(Command::DeviceRetire {
        selector: "desk".into(),
    });
    let err = outcome(&client, id).await.unwrap_err();
    assert!(err.contains("own device"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_init_sync_toggle_and_switch() {
    let f = fixture();
    let _bridge = start_bridge(&f).await;
    let client = spawn(&f);
    wait(|| async { client.snapshot().bridge.up().is_some() }).await;
    let id = client.send(Command::WorkgroupCreate {
        name: "home".into(),
        device_name: "desk".into(),
    });
    outcome(&client, id).await.unwrap();
    let _server = start_server(&f).await;
    wait(|| async { client.snapshot().server.up().is_some() }).await;
    let current = |client: &FrameworkClient| {
        client
            .snapshot()
            .server
            .up()
            .and_then(|s| s.current.clone())
    };

    let notes = f.tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let id = client.send(Command::WorkspaceInit {
        dir: notes.clone(),
        sync: true,
    });
    outcome(&client, id).await.unwrap();
    wait(|| async { current(&client).is_some_and(|w| w.sync.enabled) }).await;
    let notes = current(&client).unwrap().root;

    let id = client.send(Command::SyncDisable);
    outcome(&client, id).await.unwrap();
    wait(|| async { current(&client).is_some_and(|w| !w.sync.enabled) }).await;

    // Another folder becomes the workspace; then back to the first, which syncs again
    // because it has a sync id.
    let other = f.tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let id = client.send(Command::WorkspaceInit {
        dir: other.clone(),
        sync: false,
    });
    outcome(&client, id).await.unwrap();
    wait(|| async { current(&client).is_some_and(|w| w.root.ends_with("other")) }).await;
    let id = client.send(Command::WorkspaceSelect { dir: notes.clone() });
    outcome(&client, id).await.unwrap();
    wait(|| async { current(&client).is_some_and(|w| w.root == notes && w.sync.enabled) }).await;
}

/// A workgroup, a running server, and one synced workspace published into the ledger.
/// Returns the client and the published workspace's id.
async fn published_workspace(f: &Fixture) -> (FrameworkClient, grain_id::GrainId) {
    let client = spawn(f);
    wait(|| async { client.snapshot().bridge.up().is_some() }).await;
    let id = client.send(Command::WorkgroupCreate {
        name: "home".into(),
        device_name: "desk".into(),
    });
    outcome(&client, id).await.unwrap();
    wait(|| async { client.snapshot().server.up().is_some() }).await;

    let dir = f.tmp.path().join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    let id = client.send(Command::WorkspaceInit { dir, sync: true });
    outcome(&client, id).await.unwrap();
    wait(|| async {
        client
            .snapshot()
            .bridge
            .up()
            .is_some_and(|b| b.ledger.iter().any(|w| w.app_name == APP.app_name))
    })
    .await;
    let workspace_id = client.snapshot().bridge.up().unwrap().ledger[0].workspace_id;
    (client, workspace_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_map_creates_the_folder_and_syncs_it_under_the_ledger_id() {
    let f = fixture();
    let _bridge = start_bridge(&f).await;
    let _server = start_server(&f).await;
    let (client, workspace_id) = published_workspace(&f).await;

    // Switch this host to another workspace, so the published one is one it does not
    // serve — what "Bring to this host…" offers.
    let plain = f.tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let id = client.send(Command::WorkspaceInit {
        dir: plain,
        sync: false,
    });
    outcome(&client, id).await.unwrap();

    // A folder that does not exist yet, as `bring_target` makes it.
    let dir = f.tmp.path().join("elsewhere").join("notes");
    let id = client.send(Command::WorkspaceMap {
        workspace_id,
        dir: dir.clone(),
    });
    assert_eq!(outcome(&client, id).await, Ok(CommandOutput::Done));
    assert!(
        dir.join(format!(".{}", APP.app_name)).is_dir(),
        "the marker"
    );
    wait(|| async {
        client.snapshot().server.up().is_some_and(|s| {
            s.current
                .as_ref()
                .is_some_and(|w| w.sync.enabled && w.workspace_id == Some(workspace_id))
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn service_install_with_a_missing_binary_says_what_to_run() {
    let f = fixture();
    let mut cfg = config(&f);
    let missing = f.tmp.path().join("no-such-dir").join("sapphire-bridge");
    cfg.service_exes.bridge = missing.clone();
    let client = FrameworkClient::spawn(&tokio::runtime::Handle::current(), cfg, Arc::new(|| {}));
    let id = client.send(Command::ServiceInstall(ServiceTarget::Bridge));
    let err = outcome(&client, id).await.unwrap_err();
    assert!(err.contains("run: "), "{err}");
    assert!(err.contains(&missing.display().to_string()), "{err}");
    assert!(err.contains("service install"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn workgroup_join_pairs_with_another_bridge() {
    const NODE_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    let f = fixture();
    let net = LoopbackNetwork::new();
    // A, the inviter, on an endpoint of its own; B, the joiner, where `config` points.
    let control_a = Endpoint::in_dir("bridge-a", f.endpoints.bridge.dir.clone());
    let _a = start_bridge_as(&f, &net, NODE, "bridge-a", &control_a).await;
    let _b = start_bridge_as(&f, &net, NODE_B, "bridge", &f.endpoints.bridge).await;

    let mut cfg_a = config(&f);
    cfg_a.endpoints.bridge = control_a;
    let client_a =
        FrameworkClient::spawn(&tokio::runtime::Handle::current(), cfg_a, Arc::new(|| {}));
    wait(|| async { client_a.snapshot().bridge.up().is_some() }).await;
    let id = client_a.send(Command::WorkgroupCreate {
        name: "home".into(),
        device_name: "a".into(),
    });
    outcome(&client_a, id).await.unwrap();
    let id = client_a.send(Command::DeviceInvite {
        name: "b".into(),
        ttl_secs: Some(3600),
    });
    let Ok(CommandOutput::Ticket(ticket)) = outcome(&client_a, id).await else {
        panic!("expected a ticket");
    };

    let client_b = spawn(&f);
    wait(|| async { client_b.snapshot().bridge.up().is_some() }).await;
    let id = client_b.send(Command::WorkgroupJoin {
        ticket,
        device_name: Some("b".into()),
    });
    assert_eq!(outcome(&client_b, id).await, Ok(CommandOutput::Done));
    wait(|| async {
        client_b
            .snapshot()
            .bridge
            .up()
            .and_then(|b| b.status.workgroup.as_ref())
            .is_some_and(|w| w.name == "home")
    })
    .await;
}

/// Accept connections on `endpoint` and never answer them.
async fn hang(endpoint: &Endpoint) -> tokio::task::JoinHandle<()> {
    #[cfg(unix)]
    let listener = sapphire_ipc::bind(endpoint).await.unwrap();
    #[cfg(windows)]
    let mut listener = sapphire_ipc::bind(endpoint).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok(conn) = listener.accept().await {
            held.push(conn);
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_process_that_never_answers_reads_error_and_does_not_stall_the_loop() {
    let f = fixture();
    let _hung = hang(&f.endpoints.bridge).await;
    let mut cfg = config(&f);
    cfg.fetch_timeout = Duration::from_millis(300);
    let client = FrameworkClient::spawn(&tokio::runtime::Handle::current(), cfg, Arc::new(|| {}));
    wait(|| async {
        matches!(&client.snapshot().bridge, Conn::Error(m) if m.contains("did not answer"))
    })
    .await;
    // The loop kept ticking: the server side was fetched too, and is simply absent.
    assert!(client.snapshot().fetched);
    assert!(matches!(client.snapshot().server, Conn::Absent));
    // A command queued behind the hung bridge still gets an outcome.
    let id = client.send(Command::SyncEnable);
    let err = outcome(&client, id).await.unwrap_err();
    assert!(err.contains("not running"), "{err}");
}
