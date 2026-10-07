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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let guard = rt.enter();
    let dir = BridgeDir::at(f.tmp.path().join("bridge")).unwrap();
    let data = Endpoint::in_dir("bridge-data", f.endpoints.bridge.dir.clone());
    let bridge = Bridge::new(
        dir,
        Arc::new(LoopbackNetwork::new().transport(NODE)),
        "0.0.0",
    )
    .unwrap()
    .control_endpoint(f.endpoints.bridge.clone())
    .data_endpoint(data);
    drop(guard);
    rt.spawn(async move {
        let _ = Arc::new(bridge).run_shared(NetConfig::default()).await;
    });
    wait(|| async { sapphire_ipc::probe(&f.endpoints.bridge).await.unwrap() }).await;
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
    let id = client.send(Command::SyncEnable {
        root: f.tmp.path().into(),
    });
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
async fn workspace_init_sync_toggle_and_forget() {
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

    let dir = f.tmp.path().join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    let id = client.send(Command::WorkspaceInit {
        dir: dir.clone(),
        sync: true,
    });
    outcome(&client, id).await.unwrap();
    wait(|| async {
        client
            .snapshot()
            .server
            .up()
            .map(|s| s.workspaces.iter().any(|w| w.sync.enabled))
            == Some(true)
    })
    .await;
    let row = client.snapshot().server.up().unwrap().workspaces[0].clone();

    let id = client.send(Command::SyncDisable {
        root: row.root.clone(),
    });
    outcome(&client, id).await.unwrap();
    wait(|| async {
        client
            .snapshot()
            .server
            .up()
            .map(|s| !s.workspaces[0].sync.enabled)
            == Some(true)
    })
    .await;

    let id = client.send(Command::WorkspaceForget { id: row.id.clone() });
    outcome(&client, id).await.unwrap();
    wait(|| async {
        client
            .snapshot()
            .server
            .up()
            .map(|s| s.workspaces.is_empty())
            == Some(true)
    })
    .await;
}

/// Accept connections on `endpoint` and never answer them.
async fn hang(endpoint: &Endpoint) -> tokio::task::JoinHandle<()> {
    #[cfg(unix)]
    let mut listener = sapphire_ipc::bind(endpoint).await.unwrap();
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
    let id = client.send(Command::SyncEnable {
        root: f.tmp.path().into(),
    });
    let err = outcome(&client, id).await.unwrap_err();
    assert!(err.contains("not running"), "{err}");
}
