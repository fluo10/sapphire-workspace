//! Two complete hosts, built from the real pieces.
//!
//! A *host* is a bridge, an app server and a workspace: everything a machine runs, and
//! nothing a machine does not. Two of them on one [`LoopbackNetwork`] are the whole
//! architecture in miniature — client → app server → bridge → peer bridge → peer app
//! server → files — which is what `converge.rs` exercises.
//!
//! Every piece is in this process rather than spawned. A spawned app server finds its bridge
//! through `SAPPHIRE_RUNTIME_DIR`, and that is one variable in one environment: two hosts
//! cannot hold two values of it, so two hosts cannot be two spawned processes. The bridge's
//! own `switchboard.rs` and `wake.rs` solve it the same way.
//!
//! Each test file is its own crate, so a fixture one of them does not use is reported as
//! dead code.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use grain_id::GrainId;
use sapphire_backend::protocol as proto;
use sapphire_bridge_api::{BridgeClient, ManagedBy};
use sapphire_framework_bridge::{
    Bridge, BridgeDir, HelloTiming, LoopbackNetwork, NetConfig, Workgroup,
};
use sapphire_framework_server::{AppServer, SyncRuntime};
use sapphire_ipc::{Client, ClientInfo, Endpoint, connect_or_absent};
use sapphire_workspace::AppContext;

/// The node id of the first host: 64 lowercase hex digits, as the ledger wants them.
pub const NODE_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
/// The node id of the second host.
pub const NODE_B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
/// The node id of a third host: the middle host, or an unreachable one.
pub const NODE_S: &str = "c1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
/// The node id of a fourth host, for the star topology.
pub const NODE_C: &str = "d1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// The version every side reports, so a client and a server always agree.
pub const VERSION: &str = "0.0.0";

/// The application both hosts serve.
///
/// One name is right: the marker directory, the sync identity and the registration are all
/// keyed by it, and two hosts syncing one workspace are two servers of one application.
pub static CTX: AppContext = AppContext::new("sapphire-converge");

/// The application's directories, made once for this whole test binary.
///
/// [`AppContext`] is a process-wide `static` and its directories are first-writer-wins, so
/// one scratch tree has to serve every host and every test in this binary. Sharing it is
/// safe: a workspace's cache is `<cache>/<uuid>/`, and the uuid is derived from the root
/// path, so no two workspaces — on one host or on two — land on the same cache directory.
pub fn ctx() -> &'static AppContext {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::tempdir().expect("a scratch tree"));
    // Idempotent: only the first call takes effect, and every call passes the same paths.
    CTX.set_cache_dir(dir.path().join("cache"));
    CTX.set_data_dir(dir.path().join("data"));
    CTX.set_config_dir(dir.path().join("config"));
    &CTX
}

/// One machine: its bridge, its app server and its workspace.
pub struct Host {
    /// Kept for the host's whole life: [`Host::restart`] brings the processes back against
    /// these same directories, which is what "comes back with the same directories" means.
    /// An `Option` because `restart` moves it into the rebuilt host, and [`Drop`] forbids
    /// moving a field out directly.
    tmp: Option<tempfile::TempDir>,
    /// This host's node id, so a restart rejoins the loopback network under the same name.
    node_id: String,
    /// The name its device record carries.
    device_name: String,
    /// The workspace root. Named `ws` because every RPC that names a workspace names it.
    pub ws: PathBuf,
    /// The bridge directory and the workgroup this host founded.
    pub bridge_dir: BridgeDir,
    pub workgroup_id: GrainId,
    /// Where this host's sockets live. Reclaimed on restart, so `stop_bridge` can wait for
    /// the old one to let go of them.
    runtime_dir: PathBuf,
    /// This host's connection to its own app server: the "client" end of the picture.
    pub client: Client,
    /// The network this host was built on, so a test can read its frame counters.
    net: LoopbackNetwork,
    /// This host's connection to its own bridge, for fixtures that ask the bridge directly.
    bridge_client: Arc<BridgeClient>,
    /// The sync runtime, held so a stopped host can drop it and release the replica store.
    runtime: Option<Arc<SyncRuntime>>,
    server: Option<tokio::task::JoinHandle<sapphire_framework_server::Result<()>>>,
    /// The bridge runs on its own runtime, because a bridge is its own process.
    ///
    /// Both halves of that matter. A bridge is its own *process*: killing it must close the
    /// control connections it already accepted, or an app server would keep talking to a
    /// corpse and [`Host::stop_bridge`] would test nothing. And a runtime is the only thing
    /// that owns those connections — aborting `run` never touches the `serve_connection`
    /// tasks it spawned. So the bridge gets a runtime of its own, and stopping it means
    /// dropping that runtime (see [`tear_down`]).
    bridge: Option<tokio::runtime::Runtime>,
}

/// A fresh host, with a bridge and an app server of its own and sync wired but not enabled.
///
/// Its device has priority 0, so it never stands for election: hosts built this way sync as
/// a full mesh, which is what every test written before the star topology expects.
pub async fn start_host(net: &LoopbackNetwork, node_id: &str, device_name: &str) -> Host {
    start_host_with_priority(net, node_id, device_name, 0).await
}

/// A fresh host whose own device record carries `priority`.
///
/// The priority is written before anything copies the record, so a later [`introduce_all`]
/// carries it into the other hosts' ledgers, and before the bridge starts, so its Hellos
/// carry it from the first one.
pub async fn start_host_with_priority(
    net: &LoopbackNetwork,
    node_id: &str,
    device_name: &str,
    priority: u8,
) -> Host {
    build(
        net,
        node_id,
        device_name,
        priority,
        tempfile::tempdir().expect("a host tree"),
    )
    .await
}

/// Hosts with the given `(node id, device name, priority)`, introduced to one another with
/// one shared workspace, and sync enabled on each.
pub async fn star_hosts(net: &LoopbackNetwork, spec: &[(&str, &str, u8)]) -> Vec<Host> {
    let mut hosts = Vec::new();
    for (node, name, priority) in spec {
        hosts.push(start_host_with_priority(net, node, name, *priority).await);
    }
    let refs: Vec<&Host> = hosts.iter().collect();
    introduce_all(&refs);
    for host in &hosts {
        enable_sync(host).await;
    }
    hosts
}

/// Build (or rebuild) the processes of one host against `tmp`.
///
/// Everything on disk is left as it is: an existing workgroup is reopened rather than
/// founded, so a host that restarts keeps its device record, its ledger and its replica.
async fn build(
    net: &LoopbackNetwork,
    node_id: &str,
    device_name: &str,
    priority: u8,
    tmp: tempfile::TempDir,
) -> Host {
    let ctx = ctx();
    let runtime_dir = tmp.path().join("run");
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let bridge_dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
    let workgroup = match Workgroup::open(&bridge_dir).unwrap() {
        // `create` refuses a host that already has a workgroup, which is exactly the
        // restart case: the ledger, the node id and the device record are all still there.
        Some(existing) => existing,
        None => Workgroup::create(&bridge_dir, "converge", device_name, node_id).unwrap(),
    };
    // Written before the bridge starts, so its first Hello already carries it.
    workgroup.set_priority(device_name, priority).unwrap();

    let root = tmp.path().join("ws");
    std::fs::create_dir_all(root.join(format!(".{}", ctx.app_name))).unwrap();
    let root = root.canonicalize().unwrap();

    // The bridge: control, data and the inbound peer loop, on this host's own endpoints.
    //
    // `wake_on_sync` is off deliberately: the exe a route names would be this test binary,
    // and a bridge that started it would spawn a second copy of the test suite.
    let control = Endpoint::in_dir("bridge", runtime_dir.clone());
    let data = Endpoint::in_dir("bridge-data", runtime_dir.clone());
    let bridge = Bridge::new(
        bridge_dir.clone(),
        Arc::new(net.transport(node_id)),
        VERSION,
    )
    .unwrap()
    .net(NetConfig {
        wake_on_sync: false,
        discovery: false,
        relays: Vec::new(),
        use_default_relays: false,
        ..NetConfig::default()
    })
    .control_endpoint(control.clone())
    .data_endpoint(data)
    // Short Hello timing, so an election settles in well under a second instead of the
    // field's default.
    .hello_timing(HelloTiming {
        interval: Duration::from_millis(100),
        dead: Duration::from_millis(600),
    });
    let bridge_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime for the bridge");
    bridge_runtime.spawn(async move {
        let _ = bridge.run().await;
    });
    wait_until_listening(&control, "the bridge").await;

    // The app server's connection to its own bridge. Built explicitly rather than through
    // `BridgeClient::connect`, which would look the bridge up in the process environment —
    // one value, two hosts.
    let (control_client, _) = connect_or_absent(
        &control,
        "bridge",
        ClientInfo {
            api: sapphire_bridge_api::API_VERSION,
            ..client_info("test")
        },
    )
    .await
    .unwrap()
    .expect("the bridge is listening");
    let bridge_client = Arc::new(BridgeClient::from_client(
        Arc::new(control_client),
        runtime_dir.clone(),
    ));

    // The app server, with sync wired the way an application wires it.
    let endpoint = Endpoint::in_dir(ctx.app_name, runtime_dir.clone());
    let runtime = Arc::new(SyncRuntime::new(
        ctx,
        Arc::clone(&bridge_client),
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sapphire")),
        ManagedBy::Service,
    ));
    let server = AppServer::new(ctx, VERSION)
        .endpoint(endpoint.clone())
        .sync(Arc::clone(&runtime));
    let server_task = tokio::spawn(async move { server.run().await });
    wait_until_listening(&endpoint, "the app server").await;

    let (client, _) = connect_or_absent(&endpoint, ctx.app_name, client_info("cli"))
        .await
        .unwrap()
        .expect("the app server is listening");

    Host {
        tmp: Some(tmp),
        node_id: node_id.to_owned(),
        device_name: device_name.to_owned(),
        ws: root,
        bridge_dir,
        workgroup_id: workgroup.id,
        runtime_dir,
        client,
        net: net.clone(),
        bridge_client,
        runtime: Some(runtime),
        server: Some(server_task),
        bridge: Some(bridge_runtime),
    }
}

impl Host {
    /// This host's node id on the loopback network.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The sync runtime, while the app server runs. `None` once stopped.
    pub fn runtime(&self) -> Option<Arc<SyncRuntime>> {
        self.runtime.clone()
    }

    /// This host's connection to its own bridge.
    pub fn bridge(&self) -> &BridgeClient {
        &self.bridge_client
    }

    /// The id of this host's own device record.
    pub async fn device_id(&self) -> GrainId {
        Workgroup::open(&self.bridge_dir)
            .unwrap()
            .unwrap()
            .this_device(&self.node_id)
            .unwrap()
            .id
    }

    /// The shared workspace id, as `.<app>/sync-id` holds it.
    pub async fn workspace_id(&self) -> GrainId {
        std::fs::read_to_string(sync_id_path(self))
            .expect("the workspace has a sync-id")
            .trim()
            .parse()
            .expect("the sync-id is a grain-id")
    }

    /// Close this host's open sync sessions, as if its connections had been cut.
    pub async fn drop_connections(&self) {
        if let Some(runtime) = self.runtime() {
            runtime.drop_connections().await;
        }
    }

    /// Stop this host's app server and bridge, leaving every directory as it is.
    ///
    /// The app server is asked to exit rather than aborted: its shutdown path is what stops
    /// the watcher and closes the workspaces, and an aborted server would leave the watcher
    /// running and the replica store locked for the next start.
    pub async fn stop(&mut self) {
        let _: std::result::Result<proto::Ack, _> = self
            .client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await;
        if let Some(server) = self.server.take() {
            let _ = server.await;
        }
        // Dropping the runtime drops the replica, which drops the replica store: a redb
        // database is locked until then, and the restarted host must be able to open it.
        self.runtime = None;
        self.stop_bridge().await;
    }

    /// Bring this host back, against the same directories, with sync wired but not enabled.
    pub async fn restart(mut self, net: &LoopbackNetwork) -> Host {
        self.stop_bridge().await;
        let tmp = self
            .tmp
            .take()
            .expect("a host keeps its directories until it restarts");
        // Keep the priority already on disk rather than resetting it.
        let priority = Workgroup::open(&self.bridge_dir)
            .unwrap()
            .expect("a restarting host has a workgroup")
            .this_device(&self.node_id)
            .unwrap()
            .priority;
        build(net, &self.node_id, &self.device_name, priority, tmp).await
    }

    /// Stop this host's bridge, as if the daemon had died, and leave the app server running.
    pub async fn stop_bridge(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            tear_down(bridge);
        }
        // The socket file outlives the process that made it, exactly as it does in the
        // field. Waiting here is what lets the restarted bridge reclaim it: `bind` unlinks a
        // socket nothing answers, and until the old one stops answering it would fail.
        wait_until_not_listening(&Endpoint::in_dir("bridge", self.runtime_dir.clone())).await;
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // A test ends here, inside an async context, so the teardown goes to a thread of its
        // own — see [`tear_down`]. Detached rather than joined: this is only releasing the
        // host's own resources, and nothing afterwards depends on it.
        if let Some(bridge) = self.bridge.take() {
            std::thread::spawn(move || drop(bridge));
        }
    }
}

/// Enable sync on both hosts for one workspace, and make them able to reach each other.
///
/// The two halves of "these hosts share a workspace": the ledger, so each bridge knows the
/// other device, and the `sync-id`, so both servers agree which workspace it is. In the field
/// both arrive by pairing and by the workspace itself being synced; a test has to say so.
pub fn introduce(a: &Host, b: &Host) {
    // A workspace's identity is a grain-id in `.<app>/sync-id`, and it travels with the
    // workspace. Minting one per host would make two unrelated workspaces that never merge.
    let shared = GrainId::random();
    for host in [a, b] {
        std::fs::write(sync_id_path(host), format!("{shared}\n")).unwrap();
    }
    copy_device(a, b);
    copy_device(b, a);
}

/// Introduce every pair of `hosts`: one shared workspace identity and full ledgers.
///
/// [`introduce`] is the two-host case; this is its n-host form, so a middle host and an
/// unreachable one can be built with the same machinery.
pub fn introduce_all(hosts: &[&Host]) {
    let shared = GrainId::random();
    for host in hosts {
        std::fs::write(sync_id_path(host), format!("{shared}\n")).unwrap();
    }
    for from in hosts {
        for to in hosts {
            if !std::ptr::eq(*from, *to) {
                copy_device(from, to);
            }
        }
    }
}

/// `<root>/.<app>/sync-id`.
fn sync_id_path(host: &Host) -> PathBuf {
    host.ws.join(format!(".{}", ctx().app_name)).join("sync-id")
}

/// Give `to` the record of `from`'s own device — the same id, the same node id.
///
/// The ledger is synced, so a device travels to another host with its id intact; the id is
/// both the record's filename and the `Entry.author` of everything it wrote. Copying the
/// record says exactly that.
///
/// The copy's *name* is prefixed with `~`, the largest printable ASCII character, so it
/// sorts after the receiving host's own record — which matters to the `switchboard.rs`
/// fixtures, where both hosts are joined to one workgroup and a name that sorted first
/// would make the joiner's ledger ambiguous about which record is whose. Real resolution
/// never guesses: `Workgroup::this_device` matches on node id, which a test record copied
/// wholesale preserves.
fn copy_device(from: &Host, to: &Host) {
    let workgroup = Workgroup::open(&from.bridge_dir).unwrap().unwrap();
    let own = workgroup.this_device(&from.node_id).unwrap();
    let source = from
        .bridge_dir
        .devices_dir(from.workgroup_id)
        .join(own.file_name());
    let text = std::fs::read_to_string(&source).unwrap();
    let mut record: toml::Table = toml::from_str(&text).unwrap();
    // `~` is the largest printable ASCII character, so this sorts last whatever the name is.
    let name = record
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("peer")
        .to_owned();
    record.insert("name".into(), toml::Value::String(format!("~{name}")));

    let destination = to
        .bridge_dir
        .devices_dir(to.workgroup_id)
        .join(own.file_name());
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, toml::to_string_pretty(&record).unwrap()).unwrap();
}

/// What a client says it is. The version must match the server's, or the handshake replaces
/// what it finds.
fn client_info(kind: &str) -> ClientInfo {
    ClientInfo {
        kind: kind.to_owned(),
        version: VERSION.to_owned(),
        api: sapphire_backend::protocol::API_VERSION,
        pid: std::process::id(),
    }
}

/// Drop a bridge's runtime on a thread of its own.
///
/// Dropping a multi-threaded runtime is blocking work — it joins its workers and its
/// blocking pool — and tokio refuses to do it on an async worker ("Cannot drop a runtime in
/// a context where blocking is not allowed"). A thread of this test's own is where it
/// belongs, and it is the honest model: this stands in for the bridge process exiting, and
/// exiting is a thing a process does off to one side, not something the app server waits on.
///
/// The drop is what closes the control connections the bridge accepted. Leaving the runtime
/// merely shut down in the background keeps those `serve_connection` tasks alive, which is
/// how an app server ends up waiting forever on a bridge that is gone.
fn tear_down(runtime: tokio::runtime::Runtime) {
    std::thread::spawn(move || drop(runtime))
        .join()
        .expect("the bridge's runtime is torn down");
}

/// Wait until nothing answers on `endpoint` any more, polling rather than sleeping.
async fn wait_until_not_listening(endpoint: &Endpoint) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while sapphire_ipc::probe(endpoint).await.unwrap_or(false) {
        assert!(
            Instant::now() < deadline,
            "the bridge kept listening after it was stopped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until something listens on `endpoint`, polling rather than sleeping.
async fn wait_until_listening(endpoint: &Endpoint, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sapphire_ipc::probe(endpoint).await.unwrap_or(false) {
        assert!(Instant::now() < deadline, "{what} never started listening");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── live-propagation fixtures ───────────────────────────────────────────────

/// Enable sync on `host`'s workspace, the way a client would.
pub async fn enable_sync(host: &Host) {
    let _: proto::SyncEnableResult = host
        .client
        .call(
            proto::SYNC_ENABLE,
            proto::WsParams {
                ws: host.ws.clone(),
            },
        )
        .await
        .unwrap();
}

/// [`enable_sync`] for a host that has just come back through [`Host::restart`].
///
/// The previous app server's session tasks can still be winding down on this test's
/// runtime, holding the replica store's redb lock for a moment longer — a real process exit
/// would release it at once (#141). A failed open leaves nothing behind, so "Database
/// already open" is waited out against a deadline; any other error fails at once.
///
/// Only for restarts: on a first open the same error would be a regression of the
/// first-open race the app server guards against, and must fail the test.
pub async fn enable_sync_after_restart(host: &Host) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let answer: std::result::Result<proto::SyncEnableResult, _> = host
            .client
            .call(
                proto::SYNC_ENABLE,
                proto::WsParams {
                    ws: host.ws.clone(),
                },
            )
            .await;
        match answer {
            Ok(_) => return,
            Err(err)
                if format!("{err:?}").contains("Database already open")
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => panic!("sync.enable failed: {err:?}"),
        }
    }
}

/// Two hosts sharing one workspace, synced and introduced.
pub async fn synced_pair(net: &LoopbackNetwork) -> (Host, Host) {
    let a = start_host(net, NODE_A, "host-a").await;
    let b = start_host(net, NODE_B, "host-b").await;
    introduce(&a, &b);
    enable_sync(&a).await;
    enable_sync(&b).await;
    (a, b)
}

/// Three hosts sharing one workspace: `a`, a middle host `s`, and `b`.
pub async fn synced_triple(net: &LoopbackNetwork) -> (Host, Host, Host) {
    let a = start_host(net, NODE_A, "host-a").await;
    let s = start_host(net, NODE_S, "host-s").await;
    let b = start_host(net, NODE_B, "host-b").await;
    introduce_all(&[&a, &s, &b]);
    enable_sync(&a).await;
    enable_sync(&s).await;
    enable_sync(&b).await;
    (a, s, b)
}

/// Wait until every host holds an open live session to every peer its bridge reports
/// as connected.
///
/// "Settled" is about the dialer, not the files: the initial exchange of each session is
/// what catches a host up, and `open_live_session` returns only once both sides have said
/// `Done` and `Settled`. An open session to a peer therefore means "caught up with that
/// peer", which is the state the live-propagation tests need before they write anything.
///
/// Unreachable peers are excluded by the bridge's own `connected` report — the loopback
/// answers it from the partition's edges, so a host nobody can reach never blocks this.
pub async fn settle(hosts: &[&Host]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut settled = true;
        for host in hosts {
            let Some(runtime) = host.runtime() else {
                settled = false;
                break;
            };
            let peers = host
                .bridge_client
                .peers()
                .await
                .expect("the bridge answers peers");
            let connected: HashSet<GrainId> = peers
                .peers
                .iter()
                .filter(|p| p.connected && p.node_id != host.node_id())
                .map(|p| p.device_id)
                .collect();
            let open: HashSet<GrainId> = runtime
                .live_session_devices(&host.ws)
                .await
                .into_iter()
                .collect();
            if !connected.is_subset(&open) {
                eprintln!(
                    "NOT SETTLED on {}: connected={:?} open={:?}",
                    host.node_id(),
                    connected,
                    open
                );
                settled = false;
                break;
            }
        }
        if settled {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the hosts never settled into live sessions"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until every host's bridge reports `want` as the primary device of its workspace.
pub async fn await_primary(hosts: &[&Host], want: GrainId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut all = true;
        for host in hosts {
            let peers = host.bridge().peers().await.expect("peers");
            let ws = host.workspace_id().await;
            if peers.roles_for(ws).and_then(|r| r.primary) != Some(want) {
                all = false;
            }
        }
        if all {
            return;
        }
        if Instant::now() >= deadline {
            for host in hosts {
                eprintln!(
                    "peers of {}: {:?}",
                    host.node_id(),
                    host.bridge().peers().await
                );
            }
            panic!("the hosts never agreed on {want}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// How many writes this host has made onto loopback streams — the test's stand-in for
/// frames sent.
pub fn frames_sent(host: &Host) -> u64 {
    host.net.frames_sent(host.node_id())
}

/// Wait until every listed host has gone `quiet_for` without writing onto a loopback
/// stream, and return each one's counter as it was during that window — a stable baseline
/// to compare later counters against.
///
/// This is [`settle`]'s idea applied to silence: poll with a deadline instead of sleeping a
/// fixed span and trusting it. "Has this host stopped talking" is a claim about the storm
/// rules, but the fixed-sleep-then-compare form of it was also a claim about the machine —
/// the counter counts buffer writes ([`LoopbackNetwork::frames_sent`]), and on a loaded CI
/// runner a write belonging to an earlier round can land inside the window and read as a
/// loop. Waiting *for* quiet makes the window start when the talking stops, so stragglers
/// delay the measurement rather than corrupt it.
///
/// Panics once `deadline` passes without the counters holding still, which is what a real
/// loop looks like: a host that never stops.
pub async fn quiet(hosts: &[&Host], quiet_for: Duration, deadline: Duration) -> Vec<u64> {
    let deadline = Instant::now() + deadline;
    let mut last: Vec<u64> = hosts.iter().map(|host| frames_sent(host)).collect();
    let mut unchanged_since = Instant::now();
    loop {
        assert!(
            Instant::now() < deadline,
            "the hosts never went quiet: their counters sat at {last:?} while writes kept arriving"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let current: Vec<u64> = hosts.iter().map(|host| frames_sent(host)).collect();
        if current != last {
            last = current;
            unchanged_since = Instant::now();
            continue;
        }
        if unchanged_since.elapsed() >= quiet_for {
            return last;
        }
    }
}
