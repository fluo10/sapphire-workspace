//! The sapphire bridge: one host-wide daemon per user.
//!
//! Apps run one server per app, each owning a cache that only one process may open. The
//! bridge is the piece that sits above them: it holds this host's device identity, knows
//! which workgroups the host belongs to, and tells an app server where its peers are. It
//! never looks inside a workspace — it routes to the app server that owns one.
//!
//! See `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §2 and §4.

#![warn(missing_docs)]

mod command;
mod control;
mod data;
mod dir;
mod embed;
mod error;
mod invite;
#[cfg(feature = "node")]
mod iroh;
mod logging;
mod net;
mod pairing;
mod peer;
mod relay;
mod routes;
mod status;
#[cfg(any(test, feature = "test-util"))]
mod testing;
mod wgsync;
mod workgroup;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use grain_id::GrainId;
use sapphire_bridge_api::{BRIDGE_DATA_NAME, BRIDGE_NAME, ManagedBy, WorkspaceRegistration};
use sapphire_ipc::{Endpoint, ServerInfo};

pub use command::{BridgeCommand, bridge_service_spec};
// The CLI's `service install | uninstall | status`: re-exported so the binary needs this
// crate alone and the whole command surface sits in one place.
pub use dir::{BRIDGE_DIR_ENV, BRIDGE_FORMAT_VERSION, BridgeDir, InstanceLock};
pub use embed::{EmbedFactory, EmbedProvider};
pub use error::{Error, Result};
pub use invite::{DEFAULT_TTL, Invite, Invites, TICKET_PREFIX, Ticket};
#[cfg(feature = "node")]
pub use iroh::{IrohTransport, NodeAddr};
pub use logging::{
    BRIDGE_TARGET, DEFAULT_LOG_FILTER, LOG_FILE, LOG_KEEP, LOG_MAX_BYTES, LogGuard, install,
    install_console, install_with_limit, tail,
};
pub use net::NetConfig;
pub use pairing::{JoinRequest, JoinResponse, PAIR_ALPN, admit, join as join_pairing};
pub use peer::{BoxedStream, Inbound, PeerStream, PeerTransport, StreamRequest};
#[cfg(any(test, feature = "test-util"))]
pub use peer::{LoopbackNetwork, LoopbackTransport};
#[cfg(feature = "embedded-relay")]
pub use relay::EmbeddedRelay;
pub use relay::{EmbeddedRelayConfig, RelayConfig, TlsConfig, relays};
pub use routes::{Route, RouteTable};
pub use sapphire_framework_service::{ServiceCommand, ServiceSpec};
pub use status::{PeerStatus, STATUS_INTERVAL, StatusFile, StatusSource, StatusWriter};
#[cfg(any(test, feature = "test-util"))]
pub use testing::adopt_workgroup;
pub use wgsync::{WORKSPACE_APP_NAME, WorkgroupReplica};
pub use workgroup::{Workgroup, WorkgroupWorkspace};

use crate::control::{Owners, Wakes};
use crate::data::Tickets;

/// The switchboard: one control plane, one data plane, and the tables they share.
///
/// Build one with [`Bridge::new`], adjust it with the setters, then hand it to
/// [`Bridge::run`], which serves three loops until one of them fails:
/// - a **control** listener, answering `bridge.register` and its siblings over JSON-RPC;
/// - a **data** listener, where an app server asks for a stream by ticket or by device;
/// - an **inbound** loop, which accepts peer streams, authorizes them, and parks them for
///   the app server that owns the workspace they asked for.
///
/// The two listeners are dispatchers in the sense that matters here: the control plane never
/// looks inside a workspace, and the data plane never looks at the bytes it relays.
pub struct Bridge {
    dir: BridgeDir,
    transport: Arc<dyn PeerTransport>,
    version: &'static str,
    control: Endpoint,
    data: Endpoint,
    /// `None` means "read `net.toml` when `run` starts", which is what a real caller wants:
    /// the file may have been edited since this bridge was built. Tests set it outright.
    net: Option<NetConfig>,
    /// The routing table, held in memory rather than re-read per call: two app servers
    /// registering at the same moment would otherwise load-modify-write over each other. Only
    /// this process writes the file, so it cannot go stale behind us.
    routes: Mutex<RouteTable>,
    /// Which app servers are connected right now.
    owners: Owners,
    /// Inbound streams waiting for their owner.
    tickets: Tickets,
    /// When each application was last started by `wake_on_sync`.
    wakes: Wakes,
    /// The workgroup's own replica, once `run` has opened it. `None` until then, and for a
    /// host without a workgroup for ever.
    workgroup_replica: Mutex<Option<Arc<WorkgroupReplica>>>,
    /// The task driving the workgroup's own workspace — scanning its root and dialing its
    /// peers — while a replica is open. Aborted and replaced when the replica is.
    workgroup_driver: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Answers `embed.*`; `None` when this host has no embedding.
    embed: Option<Arc<dyn EmbedProvider>>,
}

impl Bridge {
    /// A bridge over `dir`, reaching other devices through `transport`, reporting `version`.
    ///
    /// The endpoints default to the built-in ones in this user's runtime directory, and the
    /// network configuration to whatever `net.toml` says. Tests override all three.
    pub fn new(
        dir: BridgeDir,
        transport: Arc<dyn PeerTransport>,
        version: &'static str,
    ) -> Result<Bridge> {
        let runtime = sapphire_ipc::runtime_dir()?;
        let routes = RouteTable::load(&dir.routes_toml())?;
        Ok(Bridge {
            dir,
            transport,
            version,
            control: Endpoint::in_dir(BRIDGE_NAME, runtime.clone()),
            data: Endpoint::in_dir(BRIDGE_DATA_NAME, runtime),
            net: None,
            routes: Mutex::new(routes),
            owners: Owners::default(),
            tickets: Tickets::default(),
            wakes: Wakes::default(),
            workgroup_replica: Mutex::new(None),
            workgroup_driver: Mutex::new(None),
            embed: None,
        })
    }

    /// Listen for app servers here instead of at the default control endpoint.
    pub fn control_endpoint(mut self, endpoint: Endpoint) -> Bridge {
        self.control = endpoint;
        self
    }

    /// Serve app-server streams here instead of at the default data endpoint.
    pub fn data_endpoint(mut self, endpoint: Endpoint) -> Bridge {
        self.data = endpoint;
        self
    }

    /// Use this network configuration instead of reading `net.toml`.
    pub fn net(mut self, net: NetConfig) -> Bridge {
        self.net = Some(net);
        self
    }

    /// Answer `embed.info` and `embed.embed` with this provider.
    pub fn embed_provider(mut self, provider: Arc<dyn EmbedProvider>) -> Bridge {
        self.embed = Some(provider);
        self
    }

    pub(crate) fn embedder(&self) -> Option<&Arc<dyn EmbedProvider>> {
        self.embed.as_ref()
    }

    /// The endpoints this bridge serves on: the control plane first, the data plane second.
    pub fn endpoints(&self) -> (Endpoint, Endpoint) {
        (self.control.clone(), self.data.clone())
    }

    /// Serve the control plane, the data plane and the inbound peer loop until one fails.
    ///
    /// Nothing here takes the single-instance lock: that is the caller's, because "a bridge
    /// is already running" is a normal outcome for a command and an error for a caller to
    /// interpret.
    pub async fn run(mut self) -> Result<()> {
        let net = match self.net.take() {
            Some(net) => net,
            // Read up front rather than at the first peer request, so an unreadable file is
            // reported when the bridge starts.
            None => NetConfig::load(&self.dir.net_toml())?,
        };
        Arc::new(self).run_shared(net).await
    }

    /// As [`Bridge::run`], over a bridge shared between the caller and its loops, with the
    /// network configuration already resolved by the caller.
    ///
    /// The form a fixture needs: a running bridge whose handle can still call back into it
    /// — the workgroup session the dialing side runs on its own replica.
    pub async fn run_shared(self: Arc<Self>, net: NetConfig) -> Result<()> {
        // The bridge is the app server of the workgroup's own workspace: it registers it
        // with itself, so a peer stream for it is routed like any other. Its owner is never
        // "online" in the app-server sense — the bridge serves it itself.
        if let Some(workgroup) = self.workgroup()?
            && let Err(err) = self.serve_workgroup(&workgroup)
        {
            // Error, not warn: the bridge stays up serving everything else, but nothing
            // tells the user their device list stopped propagating. This log line is
            // where a missing workgroup workspace is found.
            tracing::error!("the workgroup's own workspace will not sync: {err}");
        }
        self.serve_loops(net).await
    }

    /// Record `workgroup`'s own route and open its replica, so this running bridge serves it.
    ///
    /// The part of starting up that names the workgroup, factored out because a test-util
    /// caller can have the workgroup appear (or change) under a bridge that is already
    /// running — what a restarted process would pick up at [`Bridge::run`].
    fn serve_workgroup(&self, workgroup: &Workgroup) -> Result<()> {
        // The bridge is the app server of the workgroup's own workspace: it registers it
        // with itself, so a peer stream for it is routed like any other. Its owner is never
        // "online" in the app-server sense — the bridge serves it itself.
        self.routes
            .lock()
            .expect("routes")
            .put(Route {
                workspace_id: workgroup.id,
                app_name: wgsync::WORKSPACE_APP_NAME.to_owned(),
                root: workgroup.dir.join("root"),
                // Never started by `wake_on_sync`: the bridge *is* the owner, and it is
                // running, or this code would not be running.
                exe_path: std::env::current_exe()?,
                managed_by: ManagedBy::Service,
            })
            .map_err(|e| {
                tracing::warn!("could not record the workgroup's own route: {e}");
                e
            })?;
        let replica = self.open_workgroup_replica(workgroup)?;
        // Scan now, so a record a pairing on a peer wrote while this bridge was down is
        // recorded before the first session is served.
        if let Err(err) = replica.scan() {
            tracing::warn!("scanning the workgroup root failed: {err}");
        }
        // The driver takes over from here: it scans on every change under the root and
        // sweeps the workgroup workspace on a timer, so a write this host makes — a
        // pairing's admission, a retirement, a published workspace — reaches the running
        // bridges of the workgroup on its own.
        let driver = wgsync::spawn_driver(
            Arc::clone(&self.transport),
            workgroup.clone(),
            Arc::clone(&replica),
        )?;
        // A re-opened replica replaces the previous one — a join rewrites the workgroup
        // directory wholesale — so the task that drove it has to stop, or two drivers would
        // scan, sweep and dial the same workspace at once.
        if let Some(previous) = self
            .workgroup_driver
            .lock()
            .expect("workgroup driver")
            .replace(driver)
        {
            previous.abort();
        }
        *self.workgroup_replica.lock().expect("workgroup replica") = Some(replica);
        Ok(())
    }

    /// The three serving loops, over this shared bridge.
    async fn serve_loops(self: Arc<Self>, net: NetConfig) -> Result<()> {
        let (control_endpoint, data_endpoint) = self.endpoints();
        let bridge = self;
        let info = ServerInfo {
            version: bridge.version.to_owned(),
            api: sapphire_bridge_api::API_VERSION,
            pid: std::process::id(),
            // The bridge is not installed as a service yet; it is started on demand. A
            // client that finds a mismatched version may therefore replace it, which is the
            // right answer for something this process started.
            managed_by: ManagedBy::Spawned,
        };

        // Keep `status.json` current for exactly as long as the bridge serves: the writer's
        // handle is dropped when the loops end, which stops it and leaves the last snapshot
        // on disk. It needs the resolved `net`, which only this function still holds.
        let status = status::StatusWriter::start(
            bridge.dir.status_json(),
            Arc::new(status::BridgeSource::new(
                Arc::clone(&bridge),
                net.clone(),
                chrono::Utc::now(),
            )),
        );

        let result = tokio::select! {
            result = control::listen(Arc::clone(&bridge), control_endpoint, info) => result,
            result = data::listen(Arc::clone(&bridge), data_endpoint) => result,
            result = data::inbound(bridge, net) => result,
        };
        drop(status);
        result
    }

    // ── what the loops share ────────────────────────────────────────────────

    /// How this host reaches other devices.
    pub(crate) fn transport(&self) -> &dyn PeerTransport {
        self.transport.as_ref()
    }

    /// The version this bridge reports.
    pub(crate) fn version(&self) -> &'static str {
        self.version
    }

    /// The route for one workspace, if an app server on this host owns it.
    pub(crate) fn route(&self, workspace_id: GrainId) -> Option<Route> {
        self.routes
            .lock()
            .expect("routes")
            .get(workspace_id)
            .cloned()
    }

    /// Every route, ordered by application then workspace.
    pub(crate) fn route_entries(&self) -> Vec<Route> {
        self.routes.lock().expect("routes").entries().to_vec()
    }

    /// Replace every route belonging to `app_name`.
    ///
    /// Refuses a workspace another application already owns, without changing anything.
    pub(crate) fn replace_app(
        &self,
        app_name: &str,
        exe_path: PathBuf,
        managed_by: ManagedBy,
        workspaces: &[WorkspaceRegistration],
    ) -> Result<()> {
        self.routes
            .lock()
            .expect("routes")
            .replace_app(app_name, exe_path, managed_by, workspaces)
    }

    /// Forget one workspace. `false` if it was not there.
    pub(crate) fn remove_route(&self, workspace_id: GrainId) -> Result<bool> {
        self.routes.lock().expect("routes").remove(workspace_id)
    }

    /// The workgroup's own replica, once [`Bridge::run`] has opened it.
    ///
    /// [`Bridge::run`]: Bridge::run
    pub(crate) fn workgroup_replica(&self) -> Option<Arc<WorkgroupReplica>> {
        self.workgroup_replica
            .lock()
            .expect("workgroup replica")
            .clone()
    }

    /// Open the workgroup's own replica, naming this host's own device record as the author
    /// of its local writes.
    fn open_workgroup_replica(&self, workgroup: &Workgroup) -> Result<Arc<WorkgroupReplica>> {
        let device_id = workgroup.this_device(&self.transport.node_id())?.id;
        Ok(Arc::new(WorkgroupReplica::open(
            &self.dir, workgroup, device_id,
        )?))
    }

    /// The workgroup this host belongs to, if any.
    pub(crate) fn workgroup(&self) -> Result<Option<Workgroup>> {
        Workgroup::open(&self.dir)
    }

    /// Which app servers are connected right now.
    pub(crate) fn owners(&self) -> &Owners {
        &self.owners
    }

    /// Whether `app_name`'s server is connected right now. Test seam: the switchboard
    /// asserts on what a failed or successful registration left behind.
    #[cfg(any(test, feature = "test-util"))]
    pub fn is_app_online(&self, app_name: &str) -> bool {
        self.owners().is_online(app_name)
    }

    /// Inbound streams waiting for their owner.
    pub(crate) fn tickets(&self) -> &Tickets {
        &self.tickets
    }

    /// When each application was last started by `wake_on_sync`.
    pub(crate) fn wakes(&self) -> &Wakes {
        &self.wakes
    }

    /// Pick up a workgroup this bridge did not know about when it started.
    ///
    /// Pick up a workgroup this bridge did not know about, or one whose directory changed
    /// under it.
    ///
    /// Records the workgroup's own route and re-opens its replica. A replica already open is
    /// replaced: the workgroup it was opened against may be gone — a join replaces the
    /// directory wholesale — and its store with it. A running bridge calls this after it
    /// joins, so it serves the new workgroup without a restart; a fixture calls it after
    /// making two hosts one workgroup, which is what a restarted process would learn at
    /// [`Bridge::run`].
    pub fn refresh_workgroup(&self) -> Result<()> {
        let Some(workgroup) = self.workgroup()? else {
            return Ok(());
        };
        *self.workgroup_replica.lock().expect("workgroup replica") = None;
        self.serve_workgroup(&workgroup)
    }

    /// Run one workgroup replication session with a peer, now.
    ///
    /// Scans the workgroup root, dials `node_id` for the workgroup's own workspace, and
    /// runs the session with this host's replica — what a bridge with a change to share
    /// does on its own. A peer that cannot be dialed, or a session that fails, is the
    /// caller's error: the replica is left as it is, and the next session retries.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn sync_workgroup_now(&self, node_id: &str) -> Result<()> {
        let Some(workgroup) = self.workgroup()? else {
            return Ok(());
        };
        let Some(replica) = self.workgroup_replica() else {
            return Ok(());
        };
        replica.scan()?;
        let stream = self.transport().open(node_id, workgroup.id).await?;
        replica.session(stream).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_bridge_stays_free_of_the_workspace_and_retrieval_stacks() {
        // Global constraint of the plan: the bridge routes to the app server that owns a
        // workspace; it never looks inside one. Those crates are deliberately absent from
        // the manifest above, and this keeps them out.
        let manifest = include_str!("../Cargo.toml");
        for forbidden in [
            "sapphire-framework-workspace",
            "sapphire-framework-retrieve",
            "sapphire-framework-backend",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "sapphire-framework-bridge must not depend on {forbidden}"
            );
        }
    }
}
