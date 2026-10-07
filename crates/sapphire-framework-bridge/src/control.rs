//! The control plane: what an app server tells the bridge, and what the bridge answers.
//!
//! The bridge never looks inside a workspace, so the only thing it knows about an app server
//! is what that server registered here. `bridge.register` is the whole of that knowledge:
//! which workspaces an application owns, where they live, and how to reach its server again
//! when a peer asks for one of them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sapphire_bridge_api::{
    Ack, BRIDGE_NAME, INVITE, InviteParams, InviteResult, JOIN, JoinParams, JoinResult, PEERS,
    PeerInfo, PeersResult, REGISTER, RegisterParams, RegisterResult, RouteStatus, STATUS,
    StatusResult, UNREGISTER, UnregisterParams, WORKSPACES, WorkgroupStatus,
    WorkgroupWorkspaceInfo, WorkspacesResult,
};
use sapphire_ipc::{
    Connection, Endpoint, PeerHandle, RequestCtx, Router, RpcError, ServerInfo, serve,
};
use serde_json::Value;

use crate::Bridge;
use crate::error::{Error, Result};
use crate::invite::{DEFAULT_TTL, Invites, Ticket};
use crate::workgroup::Workgroup;

// ── who is connected ────────────────────────────────────────────────────────

/// Which app servers are connected right now.
#[derive(Debug, Default)]
pub(crate) struct Owners {
    by_app: Mutex<HashMap<String, PeerHandle>>,
}

impl Owners {
    /// Remember how to reach this app server. A second registration replaces the first.
    pub(crate) fn connect(&self, app_name: &str, peer: PeerHandle) {
        self.by_app
            .lock()
            .expect("owners")
            .insert(app_name.to_owned(), peer);
    }

    /// How to announce an incoming stream to this app, if it is connected.
    pub(crate) fn peer(&self, app_name: &str) -> Option<PeerHandle> {
        self.by_app.lock().expect("owners").get(app_name).cloned()
    }

    /// Is this app's server connected right now?
    #[cfg(any(test, feature = "test-util"))]
    pub fn is_online(&self, app_name: &str) -> bool {
        self.by_app.lock().expect("owners").contains_key(app_name)
    }

    /// Drop an app's registration when its control connection closes.
    ///
    /// Its **routes stay** in `routes.toml`: that is how the bridge knows where a workspace
    /// lives when its server is merely stopped, and how `wake_on_sync` finds it again.
    pub(crate) fn disconnect(&self, app_name: &str) {
        self.by_app.lock().expect("owners").remove(app_name);
    }
}

/// How long an application is left alone after the bridge tried to start it.
///
/// A peer that reconnects in a loop asks for the same workspace over and over, and each ask
/// finds the owner still starting up. Without a floor between attempts the bridge would fork
/// the app server once per ask — a fork bomb any device of the workgroup could trigger.
pub(crate) const WAKE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// When the bridge last tried to start each application.
///
/// Beside [`Owners`], because it is the same kind of fact about the same subject: what the
/// bridge knows about the app servers of this host, rather than about their workspaces.
#[derive(Debug, Default)]
pub(crate) struct Wakes {
    last: Mutex<HashMap<String, std::time::Instant>>,
}

impl Wakes {
    /// Whether `app_name` may be started now, recording the attempt if so.
    ///
    /// The window is [`WAKE_INTERVAL`]. Refusing is not an error: the ticket for the stream
    /// that prompted this attempt stays parked, so the owner that is already starting has
    /// that ask — and every ask that arrives while it starts — waiting for it.
    pub(crate) fn claim(&self, app_name: &str) -> bool {
        let now = std::time::Instant::now();
        let mut last = self.last.lock().expect("wakes");
        match last.get(app_name) {
            Some(previous) if now.duration_since(*previous) < WAKE_INTERVAL => false,
            _ => {
                last.insert(app_name.to_owned(), now);
                true
            }
        }
    }
}

/// The app servers one control connection has registered.
///
/// The bridge holds a [`PeerHandle`] per app, but nothing asks whether that handle still
/// leads anywhere: only the connection closing says that, and only the loop that serves the
/// connection sees it. So each connection keeps the list of what it registered, and
/// [`serve_connection`] replays the list into [`Owners::disconnect`] when the connection ends.
#[derive(Debug, Default)]
pub(crate) struct Session {
    apps: Mutex<Vec<String>>,
}

impl Session {
    /// Remember that this connection registered `app_name`.
    fn record(&self, app_name: &str) {
        let mut apps = self.apps.lock().expect("session");
        if !apps.iter().any(|a| a == app_name) {
            apps.push(app_name.to_owned());
        }
    }

    /// The app servers registered on this connection, emptying the list.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.apps.lock().expect("session"))
    }
}

// ── serving ─────────────────────────────────────────────────────────────────

/// Serve control connections until the listener fails.
pub(crate) async fn listen(
    bridge: Arc<Bridge>,
    endpoint: Endpoint,
    info: ServerInfo,
) -> Result<()> {
    #[cfg(unix)]
    let listener = sapphire_ipc::bind(&endpoint).await?;
    #[cfg(windows)]
    let mut listener = sapphire_ipc::bind(&endpoint)?;

    loop {
        let conn = listener.accept().await?;
        let bridge = Arc::clone(&bridge);
        let info = info.clone();
        tokio::spawn(serve_connection(conn, bridge, info));
    }
}

/// Serve one control connection, and drop its registrations when it closes.
///
/// **A registration lasts as long as the control connection**: when it goes, the app server
/// is no longer online, so a peer asking for one of its workspaces is told the workspace is
/// this host's but its owner is not running. The app's **routes stay** in `routes.toml`,
/// because that is how the bridge knows where a workspace lives when its server is merely
/// stopped — and how `wake_on_sync` finds it again.
pub(crate) async fn serve_connection(conn: Connection, bridge: Arc<Bridge>, info: ServerInfo) {
    let session = Arc::new(Session::default());
    let router = Arc::new(router(Arc::clone(&bridge), Arc::clone(&session)));

    let _ = serve(conn, router, BRIDGE_NAME, info).await;

    for app_name in session.take() {
        bridge.owners().disconnect(&app_name);
        tracing::debug!(app = %app_name, "an app server's control connection closed");
    }
}

/// The methods the bridge answers on the control plane.
///
/// One router per connection, because a registration belongs to the connection that made it:
/// the handlers record what they accepted in `session`, which [`serve_connection`] replays
/// when the connection ends.
fn router(bridge: Arc<Bridge>, session: Arc<Session>) -> Router {
    Router::new()
        .method(REGISTER, {
            let bridge = Arc::clone(&bridge);
            let session = Arc::clone(&session);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                let session = Arc::clone(&session);
                async move { register(&bridge, &session, ctx).await }
            }
        })
        .method(UNREGISTER, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move { unregister(&bridge, ctx).await }
            }
        })
        .method(PEERS, {
            let bridge = Arc::clone(&bridge);
            move |_| {
                let bridge = Arc::clone(&bridge);
                async move { peers(&bridge).map_err(failed).and_then(encode) }
            }
        })
        .method(STATUS, {
            let bridge = Arc::clone(&bridge);
            move |_| {
                let bridge = Arc::clone(&bridge);
                async move { status(&bridge).map_err(failed).and_then(encode) }
            }
        })
        .method(INVITE, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move { invite(&bridge, ctx).map_err(failed).and_then(encode) }
            }
        })
        .method(JOIN, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move { join(&bridge, ctx).await.map_err(failed).and_then(encode) }
            }
        })
        .method(WORKSPACES, {
            let bridge = Arc::clone(&bridge);
            move |_| {
                let bridge = Arc::clone(&bridge);
                async move { workspaces(&bridge).map_err(failed).and_then(encode) }
            }
        })
}

/// `bridge.register` — an app server announcing the workspaces it owns.
async fn register(
    bridge: &Bridge,
    session: &Session,
    ctx: RequestCtx,
) -> std::result::Result<Value, RpcError> {
    let params: RegisterParams = serde_json::from_value(ctx.params)
        .map_err(|e| RpcError::invalid_params(format!("malformed registration: {e}")))?;

    // The workgroup checks come first, before anything is changed: a registration that
    // ends in an error must leave no route behind and no owner marked online. Reading
    // these after `connect` would leave an app server announced as owning its workspaces
    // while the reply tells it the registration failed — and its announce loop would keep
    // speaking for a bridge that never accepted it.
    let workgroup = bridge
        .workgroup()
        .map_err(failed)?
        .ok_or(Error::NoWorkgroup)
        .map_err(failed)?;
    let me = workgroup
        .this_device(&bridge.transport().node_id())
        .map_err(failed)?;

    // A registration is the app server's complete current list, so this replaces whatever
    // this app owned before — and refuses a workspace another app already owns.
    bridge
        .replace_app(
            &params.app_name,
            params.exe_path.clone(),
            params.managed_by,
            &params.workspaces,
        )
        .map_err(failed)?;

    // Each registered workspace is announced to the workgroup: enabling sync on one host
    // makes the workspace visible to every other. `name` defaults to the root directory's
    // file name, the name the user already calls it by.
    if let Some(workgroup) = bridge.workgroup().map_err(failed)? {
        for registration in &params.workspaces {
            let name = registration
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| registration.workspace_id.to_string());
            workgroup
                .publish_workspace(&crate::workgroup::WorkgroupWorkspace {
                    workspace_id: registration.workspace_id,
                    app_name: params.app_name.clone(),
                    name,
                })
                .map_err(failed)?;
        }
    }

    // From here on this connection speaks for this app, until it closes.
    bridge.owners().connect(&params.app_name, ctx.peer.clone());
    session.record(&params.app_name);

    encode(RegisterResult {
        device_id: me.id,
        node_id: bridge.transport().node_id(),
        workgroup_id: workgroup.id,
    })
}

/// `bridge.unregister` — an app server giving up one workspace.
async fn unregister(bridge: &Bridge, ctx: RequestCtx) -> std::result::Result<Value, RpcError> {
    let params: UnregisterParams = serde_json::from_value(ctx.params)
        .map_err(|e| RpcError::invalid_params(format!("malformed unregistration: {e}")))?;

    // Not an error when it was not there: the caller asked for a workspace to stop being this
    // host's, which is already true.
    if !bridge.remove_route(params.workspace_id).map_err(failed)? {
        tracing::debug!(
            workspace = %params.workspace_id,
            "an app server unregistered a workspace this host did not have"
        );
    }
    encode(Ack {})
}

/// `bridge.peers` — the workgroup's devices, and which are reachable.
fn peers(bridge: &Bridge) -> Result<PeersResult> {
    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    Ok(PeersResult {
        peers: peer_infos(bridge, &workgroup)?,
    })
}

/// Every non-retired device of `workgroup`, and which are reachable.
///
/// Shared with `status.json`, which reports the same devices without a control-plane
/// caller to ask it first.
pub(crate) fn peer_infos(bridge: &Bridge, workgroup: &Workgroup) -> Result<Vec<PeerInfo>> {
    let devices = workgroup.devices()?;
    Ok(devices
        .entries()
        .iter()
        .filter(|d| !d.is_retired())
        .map(|d| {
            // A device that has never announced itself has no node id; `PeerInfo` reports
            // that as an empty string rather than a made-up one.
            let node_id = d.node_id.clone().unwrap_or_default();
            PeerInfo {
                device_id: d.id,
                name: d.name.clone(),
                connected: !node_id.is_empty() && bridge.transport().is_connected(&node_id),
                node_id,
            }
        })
        .collect())
}

/// `bridge.status` — what this bridge knows about itself.
fn status(bridge: &Bridge) -> Result<StatusResult> {
    let workgroup = bridge.workgroup()?;
    Ok(StatusResult {
        version: bridge.version().to_owned(),
        node_id: bridge.transport().node_id(),
        workgroup: workgroup.as_ref().map(workgroup_status).transpose()?,
        routes: route_statuses(bridge),
    })
}

/// What `workgroup` says about itself, as [`WorkgroupStatus`] carries it.
///
/// Shared with `status.json`, which reports the same fact without a caller to ask it first.
pub(crate) fn workgroup_status(workgroup: &Workgroup) -> Result<WorkgroupStatus> {
    let devices = workgroup
        .devices()?
        .entries()
        .iter()
        .filter(|d| !d.is_retired())
        .count();
    Ok(WorkgroupStatus {
        workgroup_id: workgroup.id,
        name: workgroup.name.clone(),
        devices,
    })
}

/// The routing table, saying of each row whether its owner is connected right now.
pub(crate) fn route_statuses(bridge: &Bridge) -> Vec<RouteStatus> {
    bridge
        .route_entries()
        .iter()
        .map(|route| RouteStatus {
            workspace_id: route.workspace_id,
            app_name: route.app_name.clone(),
            root: route.root.clone(),
            owner_online: bridge.owners().peer(&route.app_name).is_some(),
        })
        .collect()
}

/// `bridge.invite` — create an invite, and hand back the ticket.
///
/// The bridge composes the ticket because the ticket names the address a joiner must dial,
/// and only the process holding the bound endpoint knows it. Writing the invite is a local
/// write to `invites.toml`: any process holding the lock may issue one, and the one that
/// answers the pairing is not necessarily the one that created it.
fn invite(bridge: &Bridge, ctx: RequestCtx) -> Result<InviteResult> {
    let params: InviteParams = serde_json::from_value(ctx.params)
        .map_err(|e| Error::Config(format!("malformed invite: {e}")))?;

    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    if let Some(selector) = &params.workgroup {
        // One workgroup per host in this release, and the selector rule is name-first, the
        // same one workspaces use (spec §3.5).
        if selector != &workgroup.name && selector != &workgroup.id.to_string() {
            return Err(Error::Config(format!(
                "this host's workgroup is {} ({}), not {selector}",
                workgroup.name, workgroup.id
            )));
        }
    }

    let ttl = params
        .ttl
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_TTL);
    let (invite, secret) =
        Invites::load(&bridge.dir.root.join("invites.toml"))?.create(&params.name, ttl)?;
    let ticket = Ticket {
        workgroup_id: workgroup.id,
        node_addr: bridge.transport().ticket_addr()?,
        secret,
        expires_at: invite.expires_at,
    };
    Ok(InviteResult {
        ticket: ticket.encode(),
    })
}

/// `bridge.join` — join the workgroup a ticket names, as this device.
///
/// The running bridge does the dialing, because it holds the endpoint the pairing runs over
/// and because it is the process that must go on to serve the new workgroup. A join replaces
/// this host's workgroup directory, so the workgroup's own replica is re-opened afterwards:
/// the one the bridge was serving belonged to a workgroup this host no longer is.
async fn join(bridge: &Bridge, ctx: RequestCtx) -> Result<JoinResult> {
    let params: JoinParams = serde_json::from_value(ctx.params)
        .map_err(|e| Error::Config(format!("malformed join: {e}")))?;
    let ticket = Ticket::decode(&params.ticket)?;
    let device_name = params.device_name.unwrap_or_else(host_name);

    // `Workgroup::join` refuses when a workgroup already exists; this reports the same fact
    // without dialing, so the user gets the answer before any network work.
    let workgroup = Workgroup::join(&bridge.dir, &ticket, &device_name, bridge.transport()).await?;
    let result = JoinResult {
        workgroup_id: workgroup.id,
        workgroup_name: workgroup.name.clone(),
        device_id: workgroup.this_device(&bridge.transport().node_id())?.id,
    };

    // Only now does the bridge serve it: a join that failed above must not leave a route or
    // replica naming a workgroup this host never joined.
    bridge.refresh_workgroup()?;
    Ok(result)
}

/// `bridge.workspaces` — what the workgroup holds.
///
/// The bridge is the app server of the workgroup's own workspace, so this is the list it
/// already keeps; it is read-only, per spec §1.
fn workspaces(bridge: &Bridge) -> Result<WorkspacesResult> {
    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    let workspaces = workgroup
        .workspaces()?
        .into_iter()
        .map(|w| WorkgroupWorkspaceInfo {
            workspace_id: w.workspace_id,
            app_name: w.app_name,
            name: w.name,
        })
        .collect();
    Ok(WorkspacesResult { workspaces })
}

/// This host's name, for a device record that was not given one.
///
/// There is no portable API for it, so this reads what the shell and Windows both export and
/// falls back to a fixed name rather than failing a join over a cosmetic default.
fn host_name() -> String {
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(name) = std::env::var(var)
            && !name.is_empty()
        {
            return name;
        }
    }
    "device".to_owned()
}

/// Serialise a handler's answer.
fn encode<T: serde::Serialize>(value: T) -> std::result::Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(e.to_string()))
}

/// A bridge failure, as the JSON-RPC error that carries it back.
///
/// The message is the bridge's own — "no app server on this host owns workspace …", "this
/// host has not joined a workgroup" — because the app server is the one caller that can act
/// on it. It travels as an internal error: the request was well-formed, and the bridge failed
/// to do what was asked.
fn failed(err: Error) -> RpcError {
    RpcError::internal(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::LoopbackNetwork;
    use crate::{BridgeDir, NetConfig};
    use sapphire_ipc::{Client, ClientInfo};
    use std::path::PathBuf;

    /// A bridge over a fresh directory, whose endpoints point nowhere in particular.
    fn bridge(tmp: &tempfile::TempDir) -> Arc<Bridge> {
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        crate::workgroup::Workgroup::create(&dir, "test", "host-a", "aaaa").unwrap();
        Arc::new(
            Bridge::new(
                dir,
                Arc::new(LoopbackNetwork::new().transport("aaaa")),
                "0.0.0",
            )
            .unwrap()
            .net(NetConfig::default()),
        )
    }

    fn registration(ws: grain_id::GrainId) -> RegisterParams {
        RegisterParams {
            app_name: "test-app".into(),
            exe_path: PathBuf::from("/bin/true"),
            managed_by: sapphire_bridge_api::ManagedBy::Service,
            workspaces: vec![sapphire_bridge_api::WorkspaceRegistration {
                workspace_id: ws,
                root: PathBuf::from("/a"),
            }],
        }
    }

    fn info() -> ServerInfo {
        ServerInfo {
            version: "0.0.0".into(),
            api: sapphire_bridge_api::API_VERSION,
            pid: 1,
            managed_by: sapphire_bridge_api::ManagedBy::Spawned,
        }
    }

    /// A control connection to `bridge`, served in the background. Returns the client and
    /// the serving task.
    async fn connect(bridge: &Arc<Bridge>) -> (Client, tokio::task::JoinHandle<()>) {
        let (client_conn, server_conn) = Connection::pair();
        let serving = {
            let bridge = Arc::clone(bridge);
            tokio::spawn(serve_connection(server_conn, bridge, info()))
        };
        let (client, _) = Client::handshake(
            client_conn,
            BRIDGE_NAME,
            ClientInfo {
                kind: "test".into(),
                version: "0.0.0".into(),
                api: sapphire_bridge_api::API_VERSION,
                pid: std::process::id(),
            },
        )
        .await
        .unwrap();
        (client, serving)
    }

    async fn call(client: &Client, method: &str, params: Value) -> Value {
        client.call::<_, Value>(method, params).await.unwrap()
    }

    #[test]
    fn a_wake_claim_is_granted_once_per_window_and_per_application() {
        let wakes = Wakes::default();
        assert!(wakes.claim("test-app"), "the first attempt is granted");
        assert!(
            !wakes.claim("test-app"),
            "a second attempt inside the window must be refused"
        );
        assert!(
            wakes.claim("other-app"),
            "the window is per application, not global"
        );

        // After the window, the owner may be tried again: it may have crashed on start.
        wakes
            .last
            .lock()
            .unwrap()
            .insert("test-app".into(), std::time::Instant::now() - WAKE_INTERVAL);
        assert!(wakes.claim("test-app"));
    }

    #[tokio::test]
    async fn a_registration_lasts_exactly_as_long_as_its_control_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let ws = grain_id::GrainId::random();
        let (client, serving) = connect(&bridge).await;

        let registered = call(
            &client,
            REGISTER,
            serde_json::to_value(registration(ws)).unwrap(),
        )
        .await;
        assert_eq!(
            serde_json::from_value::<RegisterResult>(registered)
                .unwrap()
                .workgroup_id,
            bridge.workgroup().unwrap().unwrap().id
        );
        assert!(bridge.owners().is_online("test-app"));

        // The app server goes away.
        drop(client);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while bridge.owners().is_online("test-app") {
            assert!(
                std::time::Instant::now() < deadline,
                "the registration must end with the connection"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        serving.await.unwrap();

        // …but the route stays, which is how a stopped owner is found again.
        let route = bridge
            .route(ws)
            .expect("the route must outlive the connection");
        assert_eq!(route.app_name, "test-app");
    }

    #[tokio::test]
    async fn status_reports_routes_and_whether_their_owners_are_connected() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let ws = grain_id::GrainId::random();
        let (client, _serving) = connect(&bridge).await;

        call(
            &client,
            REGISTER,
            serde_json::to_value(registration(ws)).unwrap(),
        )
        .await;

        let reported: StatusResult =
            serde_json::from_value(call(&client, STATUS, serde_json::json!({})).await).unwrap();
        assert_eq!(reported.node_id, "aaaa");
        assert_eq!(reported.version, "0.0.0");
        assert_eq!(reported.workgroup.expect("a workgroup").devices, 1);
        assert_eq!(reported.routes.len(), 1);
        assert_eq!(reported.routes[0].workspace_id, ws);
        assert!(
            reported.routes[0].owner_online,
            "the app that just registered is connected"
        );

        let peers: PeersResult =
            serde_json::from_value(call(&client, PEERS, serde_json::json!({})).await).unwrap();
        assert_eq!(peers.peers.len(), 1);
        assert_eq!(peers.peers[0].name, "host-a");
        assert_eq!(peers.peers[0].node_id, "aaaa");
        assert!(
            peers.peers[0].connected,
            "the loopback knows the node it was made from"
        );

        // Unregistering takes the route away.
        call(
            &client,
            UNREGISTER,
            serde_json::json!({ "workspace_id": ws }),
        )
        .await;
        let reported: StatusResult =
            serde_json::from_value(call(&client, STATUS, serde_json::json!({})).await).unwrap();
        assert!(reported.routes.is_empty());
    }

    #[tokio::test]
    async fn a_workspace_another_app_owns_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let ws = grain_id::GrainId::random();
        let (client, _serving) = connect(&bridge).await;

        call(
            &client,
            REGISTER,
            serde_json::to_value(registration(ws)).unwrap(),
        )
        .await;

        let mut other = registration(ws);
        other.app_name = "other-app".into();
        let err = client
            .call::<_, Value>(REGISTER, serde_json::to_value(other).unwrap())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("test-app"),
            "the refusal must name the app that owns it: {err}"
        );
        assert!(!bridge.owners().is_online("other-app"));
    }
}
