//! The control plane: what an app server tells the bridge, and what the bridge answers.
//!
//! The bridge never looks inside a workspace, so the only thing it knows about an app server
//! is what that server registered here. `bridge.register` is the whole of that knowledge:
//! which workspaces an application owns, where they live, and how to reach its server again
//! when a peer asks for one of them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sapphire_bridge_api::{
    Ack, BRIDGE_NAME, DEVICE_PRIORITY_SET, DEVICE_RETIRE, DevicePrioritySetParams,
    DevicePrioritySetResult, DeviceRetireParams, DeviceRetireResult, EMBED, EMBED_DEVICE_SET,
    EMBED_INFO, EMBED_KEY_CLEAR, EMBED_KEY_SET, EMBED_MODEL_SET, EMBED_SETTINGS,
    EmbedDeviceSetParams, EmbedInfoResult, EmbedKeySetParams, EmbedModelSetParams, EmbedParams,
    EmbedResult, EmbedSettingsResult, INVITE, InviteParams, InviteResult, JOIN, JoinParams,
    JoinResult, PEERS, PeerInfo, PeersResult, REGISTER, RegisterParams, RegisterResult,
    RouteStatus, STATUS, StatusResult, UNREGISTER, UnregisterParams, WORKGROUP_CREATE, WORKSPACES,
    WorkgroupCreateParams, WorkgroupCreateResult, WorkgroupStatus, WorkgroupWorkspaceInfo,
    WorkspaceRoles, WorkspacesResult,
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
        .method(EMBED_INFO, {
            let bridge = Arc::clone(&bridge);
            move |_| {
                let bridge = Arc::clone(&bridge);
                async move { encode(embed_info(&bridge)) }
            }
        })
        .method(EMBED, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move { embed(&bridge, ctx).await }
            }
        })
        .method(EMBED_SETTINGS, {
            let bridge = Arc::clone(&bridge);
            move |_| {
                let bridge = Arc::clone(&bridge);
                async move {
                    embed_settings_report(&bridge)
                        .map_err(failed)
                        .and_then(encode)
                }
            }
        })
        .method(EMBED_MODEL_SET, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move {
                    let params: EmbedModelSetParams = parse(ctx, "embed.model_set")?;
                    crate::embed_settings::set_model(&bridge.dir, params).map_err(failed)?;
                    embed_settings_report(&bridge)
                        .map_err(failed)
                        .and_then(encode)
                }
            }
        })
        .method(EMBED_DEVICE_SET, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move {
                    let params: EmbedDeviceSetParams = parse(ctx, "embed.device_set")?;
                    crate::embed_settings::set_device(&bridge.dir, params.slot, params.enabled)
                        .map_err(failed)?;
                    embed_settings_report(&bridge)
                        .map_err(failed)
                        .and_then(encode)
                }
            }
        })
        .method(EMBED_KEY_SET, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move {
                    let params: EmbedKeySetParams = parse(ctx, "embed.key_set")?;
                    crate::embed_settings::set_key(&bridge.dir, &params.key).map_err(failed)?;
                    embed_settings_report(&bridge)
                        .map_err(failed)
                        .and_then(encode)
                }
            }
        })
        .method(EMBED_KEY_CLEAR, {
            let bridge = Arc::clone(&bridge);
            move |_| {
                let bridge = Arc::clone(&bridge);
                async move {
                    crate::embed_settings::clear_key(&bridge.dir).map_err(failed)?;
                    embed_settings_report(&bridge)
                        .map_err(failed)
                        .and_then(encode)
                }
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
        .method(WORKGROUP_CREATE, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move {
                    workgroup_create(&bridge, ctx)
                        .map_err(failed)
                        .and_then(encode)
                }
            }
        })
        .method(DEVICE_RETIRE, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move { device_retire(&bridge, ctx).map_err(failed).and_then(encode) }
            }
        })
        .method(DEVICE_PRIORITY_SET, {
            let bridge = Arc::clone(&bridge);
            move |ctx| {
                let bridge = Arc::clone(&bridge);
                async move {
                    device_priority_set(&bridge, ctx)
                        .map_err(failed)
                        .and_then(encode)
                }
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
    let roles = bridge
        .roles()
        .into_iter()
        .map(|(workspace_id, r)| WorkspaceRoles {
            workspace_id,
            primary: r.primary,
            secondary: r.secondary,
        })
        .collect();
    Ok(PeersResult {
        peers: peer_infos(bridge, &workgroup)?,
        roles,
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
                priority: d.priority,
                availability: bridge.neighbours().get(d.id).and_then(|h| h.availability),
                node_id,
            }
        })
        .collect())
}

/// `embed.info` — whether this host embeds, and with which model.
pub(crate) fn embed_info(bridge: &Bridge) -> EmbedInfoResult {
    let provider = bridge.embedder();
    let info = provider.as_ref().and_then(|p| p.info());
    EmbedInfoResult {
        enabled: info.is_some(),
        loaded: info.is_some() && provider.is_some_and(|p| p.loaded()),
        model: info,
        note: bridge.embed_note(),
    }
}

/// Every `embed.*` settings method's answer: the settings read again, the provider rebuilt
/// if they changed, and what `embed.info` says now.
fn embed_settings_report(bridge: &Bridge) -> Result<EmbedSettingsResult> {
    let resolved = bridge.reload_embed()?;
    Ok(resolved.report(embed_info(bridge)))
}

/// A method's parameters, or the error that says they are malformed.
fn parse<T: serde::de::DeserializeOwned>(
    ctx: RequestCtx,
    method: &str,
) -> std::result::Result<T, RpcError> {
    serde_json::from_value(ctx.params)
        .map_err(|e| RpcError::invalid_params(format!("malformed {method}: {e}")))
}

/// `embed.embed` — one vector per text, in order. A provider failure travels with its own
/// message.
async fn embed(bridge: &Bridge, ctx: RequestCtx) -> std::result::Result<Value, RpcError> {
    let params: EmbedParams = serde_json::from_value(ctx.params)
        .map_err(|e| RpcError::invalid_params(format!("malformed embed request: {e}")))?;
    let (provider, info) = match bridge.embedder().map(|p| {
        let info = p.info();
        (p, info)
    }) {
        Some((p, Some(info))) => (p, info),
        _ => {
            return Err(failed(Error::Config(
                "embedding is not enabled on this host".to_owned(),
            )));
        }
    };
    let vectors = provider
        .embed(params.texts)
        .await
        .map_err(RpcError::internal)?;
    encode(EmbedResult {
        model: info.model,
        dimension: info.dimension,
        vectors,
    })
}

/// `bridge.status` — what this bridge knows about itself.
fn status(bridge: &Bridge) -> Result<StatusResult> {
    let workgroup = bridge.workgroup()?;
    Ok(StatusResult {
        version: bridge.version().to_owned(),
        node_id: bridge.transport().node_id(),
        workgroup: workgroup.as_ref().map(workgroup_status).transpose()?,
        embedding: Some(embed_info(bridge)),
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

/// `bridge.workgroup_create` — found a workgroup on this host, as its first device.
///
/// The running bridge does it, so the process that will serve the workgroup is the one that
/// wrote it, and it starts serving at once — the same `refresh_workgroup` a join ends with.
fn workgroup_create(bridge: &Bridge, ctx: RequestCtx) -> Result<WorkgroupCreateResult> {
    let params: WorkgroupCreateParams = serde_json::from_value(ctx.params)
        .map_err(|e| Error::Config(format!("malformed workgroup_create: {e}")))?;
    let node_id = bridge.transport().node_id();
    let workgroup = Workgroup::create(&bridge.dir, &params.name, &params.device_name, &node_id)?;
    let device_id = workgroup.this_device(&node_id)?.id;
    bridge.refresh_workgroup()?;
    Ok(WorkgroupCreateResult {
        workgroup_id: workgroup.id,
        name: workgroup.name,
        device_id,
    })
}

/// `bridge.device_retire` — retire a device of this host's workgroup.
///
/// Takes effect at once: the ledger is re-read on every authorization.
fn device_retire(bridge: &Bridge, ctx: RequestCtx) -> Result<DeviceRetireResult> {
    let params: DeviceRetireParams = serde_json::from_value(ctx.params)
        .map_err(|e| Error::Config(format!("malformed device_retire: {e}")))?;
    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    let device = workgroup.retire_device(&params.selector, &bridge.transport().node_id())?;
    Ok(DeviceRetireResult {
        device_id: device.id,
        name: device.name,
    })
}

/// `bridge.device_priority_set` — set a device's election priority.
///
/// Refuses a retired device. Takes effect at this host's next election round; peers learn
/// it when the ledger change syncs to them.
fn device_priority_set(bridge: &Bridge, ctx: RequestCtx) -> Result<DevicePrioritySetResult> {
    let params: DevicePrioritySetParams = serde_json::from_value(ctx.params)
        .map_err(|e| Error::Config(format!("malformed device_priority_set: {e}")))?;
    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    let device = workgroup.set_priority(&params.selector, params.priority)?;
    Ok(DevicePrioritySetResult {
        device_id: device.id,
        name: device.name,
        priority: device.priority,
    })
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

    /// A bridge over a fresh directory with no workgroup yet.
    fn bare_bridge(tmp: &tempfile::TempDir) -> Arc<Bridge> {
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
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

    #[tokio::test]
    async fn workgroup_create_founds_one_and_status_shows_it_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bare_bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;

        let created: sapphire_bridge_api::WorkgroupCreateResult = serde_json::from_value(
            call(
                &client,
                sapphire_bridge_api::WORKGROUP_CREATE,
                serde_json::json!({ "name": "home", "device_name": "desk" }),
            )
            .await,
        )
        .unwrap();
        assert_eq!(created.name, "home");

        let reported: StatusResult =
            serde_json::from_value(call(&client, STATUS, serde_json::json!({})).await).unwrap();
        let wg = reported.workgroup.expect("the new workgroup");
        assert_eq!(wg.workgroup_id, created.workgroup_id);
        assert_eq!(wg.devices, 1);

        let err = client
            .call::<_, Value>(
                sapphire_bridge_api::WORKGROUP_CREATE,
                serde_json::json!({ "name": "again", "device_name": "desk" }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already belongs"), "{err}");
    }

    #[tokio::test]
    async fn device_retire_refuses_this_hosts_own_device() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp); // founded as "host-a" with node "aaaa"
        let (client, _serving) = connect(&bridge).await;
        let err = client
            .call::<_, Value>(
                sapphire_bridge_api::DEVICE_RETIRE,
                serde_json::json!({ "selector": "host-a" }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("own device"), "{err}");
    }

    #[tokio::test]
    async fn device_retire_takes_another_device_out_of_peers() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        bridge
            .workgroup()
            .unwrap()
            .unwrap()
            .devices()
            .unwrap()
            .add("laptop", Some("bbbb".into()), None)
            .unwrap();
        let (client, _serving) = connect(&bridge).await;

        let retired: sapphire_bridge_api::DeviceRetireResult = serde_json::from_value(
            call(
                &client,
                sapphire_bridge_api::DEVICE_RETIRE,
                serde_json::json!({ "selector": "laptop" }),
            )
            .await,
        )
        .unwrap();
        assert_eq!(retired.name, "laptop");
        let peers: PeersResult =
            serde_json::from_value(call(&client, PEERS, serde_json::json!({})).await).unwrap();
        assert!(peers.peers.iter().all(|p| p.name != "laptop"));
    }

    #[tokio::test]
    async fn device_priority_set_changes_the_ledger_and_peers_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;

        let set: sapphire_bridge_api::DevicePrioritySetResult = serde_json::from_value(
            call(
                &client,
                sapphire_bridge_api::DEVICE_PRIORITY_SET,
                serde_json::json!({ "selector": "host-a", "priority": 7 }),
            )
            .await,
        )
        .unwrap();
        assert_eq!(set.priority, 7);

        let peers: PeersResult =
            serde_json::from_value(call(&client, PEERS, serde_json::json!({})).await).unwrap();
        assert_eq!(
            peers
                .peers
                .iter()
                .find(|p| p.name == "host-a")
                .unwrap()
                .priority,
            7
        );
    }

    #[tokio::test]
    async fn device_priority_set_refuses_a_retired_device() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let wg = bridge.workgroup().unwrap().unwrap();
        wg.devices()
            .unwrap()
            .add("laptop", Some("bbbb".into()), None)
            .unwrap();
        wg.retire_device("laptop", "aaaa").unwrap();
        let (client, _serving) = connect(&bridge).await;

        let err = client
            .call::<_, Value>(
                sapphire_bridge_api::DEVICE_PRIORITY_SET,
                serde_json::json!({ "selector": "laptop", "priority": 3 }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("retired"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_bridges_elect_the_same_primary_device() {
        let net = LoopbackNetwork::new();
        let (ta, tb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let dir_a = BridgeDir::at(ta.path().join("bridge")).unwrap();
        let dir_b = BridgeDir::at(tb.path().join("bridge")).unwrap();
        let wa = crate::workgroup::Workgroup::create(&dir_a, "test", "host-a", "aaaa").unwrap();
        wa.devices()
            .unwrap()
            .add("host-b", Some("bbbb".into()), None)
            .unwrap();
        wa.set_priority("host-a", 5).unwrap();
        crate::testing::adopt_workgroup(&dir_b, &wa).unwrap();
        let timing = crate::hello::HelloTiming {
            interval: std::time::Duration::from_millis(50),
            dead: std::time::Duration::from_millis(300),
        };
        let make = |dir: BridgeDir, node: &str| {
            Arc::new(
                Bridge::new(dir, Arc::new(net.transport(node)), "0.0.0")
                    .unwrap()
                    .net(NetConfig::default())
                    .hello_timing(timing),
            )
        };
        let (a, b) = (make(dir_a, "aaaa"), make(dir_b, "bbbb"));
        let ws = grain_id::GrainId::random();
        let mut keep = Vec::new();
        for bridge in [&a, &b] {
            tokio::spawn(crate::hello::run(Arc::clone(bridge)));
            tokio::spawn(crate::data::inbound(
                Arc::clone(bridge),
                NetConfig::default(),
            ));
            let (client, serving) = connect(bridge).await;
            call(
                &client,
                REGISTER,
                serde_json::to_value(registration(ws)).unwrap(),
            )
            .await;
            keep.push((client, serving)); // the registration lasts as long as the connection
        }
        let a_id = wa.this_device("aaaa").unwrap().id;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (ra, rb) = (a.roles(), b.roles());
            if ra.get(&ws).and_then(|r| r.primary) == Some(a_id)
                && rb.get(&ws).and_then(|r| r.primary) == Some(a_id)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no agreement: {ra:?} / {rb:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
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

    struct FakeProvider;

    #[async_trait::async_trait]
    impl crate::EmbedProvider for FakeProvider {
        fn info(&self) -> Option<sapphire_bridge_api::EmbedModelInfo> {
            Some(sapphire_bridge_api::EmbedModelInfo {
                model: "fake".into(),
                dimension: 2,
                template_version: 1,
                revision: None,
                max_tokens: None,
            })
        }
        fn loaded(&self) -> bool {
            true
        }
        async fn embed(&self, texts: Vec<String>) -> std::result::Result<Vec<Vec<f32>>, String> {
            if texts.iter().any(|t| t == "boom") {
                return Err("model failed".into());
            }
            Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
        }
    }

    /// As [`bridge`], with the fake embedding provider installed.
    fn embedding_bridge(tmp: &tempfile::TempDir) -> Arc<Bridge> {
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        crate::workgroup::Workgroup::create(&dir, "test", "host-a", "aaaa").unwrap();
        Arc::new(
            Bridge::new(
                dir,
                Arc::new(LoopbackNetwork::new().transport("aaaa")),
                "0.0.0",
            )
            .unwrap()
            .net(NetConfig::default())
            .embed_provider(Arc::new(FakeProvider)),
        )
    }

    #[tokio::test]
    async fn embed_info_without_a_provider_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;
        let info: sapphire_bridge_api::EmbedInfoResult = serde_json::from_value(
            call(
                &client,
                sapphire_bridge_api::EMBED_INFO,
                serde_json::json!({}),
            )
            .await,
        )
        .unwrap();
        assert!(!info.enabled);
        assert!(info.model.is_none());
        assert!(!info.loaded);
    }

    #[tokio::test]
    async fn embed_without_a_provider_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;
        let err = client
            .call::<_, Value>(
                sapphire_bridge_api::EMBED,
                serde_json::json!({"texts": ["a"]}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("embedding is not enabled"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn embed_info_reports_the_provider_model() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = embedding_bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;
        let info: sapphire_bridge_api::EmbedInfoResult = serde_json::from_value(
            call(
                &client,
                sapphire_bridge_api::EMBED_INFO,
                serde_json::json!({}),
            )
            .await,
        )
        .unwrap();
        assert!(info.enabled);
        assert!(info.loaded);
        let model = info.model.unwrap();
        assert_eq!(model.model, "fake");
        assert_eq!(model.dimension, 2);
        assert_eq!(model.template_version, 1);
    }

    #[tokio::test]
    async fn embed_returns_one_vector_per_text_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = embedding_bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;
        let result: sapphire_bridge_api::EmbedResult = serde_json::from_value(
            call(
                &client,
                sapphire_bridge_api::EMBED,
                serde_json::json!({"texts": ["a", "abc", ""]}),
            )
            .await,
        )
        .unwrap();
        assert_eq!(result.model, "fake");
        assert_eq!(result.dimension, 2);
        assert_eq!(
            result.vectors,
            vec![vec![1.0, 1.0], vec![3.0, 1.0], vec![0.0, 1.0]]
        );
    }

    #[tokio::test]
    async fn a_provider_error_is_an_rpc_error_with_its_message() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = embedding_bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;
        let err = client
            .call::<_, Value>(
                sapphire_bridge_api::EMBED,
                serde_json::json!({"texts": ["ok", "boom"]}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("model failed"), "{err}");
    }

    #[tokio::test]
    async fn status_reports_embedding() {
        let tmp = tempfile::tempdir().unwrap();
        let with_embed = embedding_bridge(&tmp);
        let (client, _serving) = connect(&with_embed).await;
        let reported: StatusResult =
            serde_json::from_value(call(&client, STATUS, serde_json::json!({})).await).unwrap();
        let embedding = reported.embedding.expect("an embedding line");
        assert!(embedding.enabled);
        assert_eq!(embedding.model.unwrap().model, "fake");

        let tmp2 = tempfile::tempdir().unwrap();
        let plain = bridge(&tmp2);
        let (client, _serving) = connect(&plain).await;
        let reported: StatusResult =
            serde_json::from_value(call(&client, STATUS, serde_json::json!({})).await).unwrap();
        assert!(!reported.embedding.expect("always reported").enabled);
    }

    /// Reports the model of the configuration it was built for.
    struct ConfiguredProvider(sapphire_bridge_api::EmbedModelInfo);

    #[async_trait::async_trait]
    impl crate::EmbedProvider for ConfiguredProvider {
        fn info(&self) -> Option<sapphire_bridge_api::EmbedModelInfo> {
            Some(self.0.clone())
        }
        fn loaded(&self) -> bool {
            false
        }
        async fn embed(&self, texts: Vec<String>) -> std::result::Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|_| vec![0.0; self.0.dimension as usize])
                .collect())
        }
    }

    /// As [`bridge`], building providers from the settings; the counter counts builds.
    fn settings_bridge(
        tmp: &tempfile::TempDir,
        avx2: bool,
    ) -> (Arc<Bridge>, Arc<std::sync::atomic::AtomicUsize>) {
        let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&builds);
        let factory: crate::EmbedFactory = Arc::new(move |config, _dir| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (model, dimension) = match config {
                crate::EmbedConfig::Local { model, .. } => (model.model.clone(), model.dimension),
                crate::EmbedConfig::Remote { model, .. } => (model.model.clone(), model.dimension),
            };
            Some(
                Arc::new(ConfiguredProvider(sapphire_bridge_api::EmbedModelInfo {
                    model,
                    dimension,
                    template_version: 0,
                    revision: None,
                    max_tokens: None,
                })) as Arc<dyn crate::EmbedProvider>,
            )
        });
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        crate::workgroup::Workgroup::create(&dir, "test", "host-a", "aaaa").unwrap();
        let bridge = Bridge::new(
            dir,
            Arc::new(LoopbackNetwork::new().transport("aaaa")),
            "0.0.0",
        )
        .unwrap()
        .net(NetConfig::default())
        .embed_factory(factory)
        .assume_avx2(avx2);
        (Arc::new(bridge), builds)
    }

    fn remote_model(model: &str) -> sapphire_bridge_api::RemoteModel {
        sapphire_bridge_api::RemoteModel {
            endpoint: "https://api.example.com".into(),
            model: model.into(),
            dimension: 8,
        }
    }

    #[tokio::test]
    async fn settings_calls_rebuild_the_provider_only_when_they_change() {
        use sapphire_bridge_api::{EmbedNote, EmbedSettingsResult, Slot, SlotModel};
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let (bridge, builds) = settings_bridge(&tmp, true);
        bridge.reload_embed().unwrap();
        let (client, _serving) = connect(&bridge).await;

        let info: EmbedInfoResult =
            serde_json::from_value(call(&client, EMBED_INFO, serde_json::json!({})).await).unwrap();
        assert!(!info.enabled);
        assert_eq!(info.note, Some(EmbedNote::NotConfigured));

        let set: EmbedSettingsResult = serde_json::from_value(
            call(
                &client,
                EMBED_MODEL_SET,
                serde_json::to_value(EmbedModelSetParams {
                    slot: Slot::Remote,
                    model: Some(SlotModel::Remote(remote_model("m1"))),
                })
                .unwrap(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(set.active, Some(Slot::Remote));
        assert_eq!(set.info.model.as_ref().unwrap().model, "m1");
        assert_eq!(set.info.note, Some(EmbedNote::KeyMissing));
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        // Reading changes nothing, and neither does a reload over the same files.
        call(&client, EMBED_SETTINGS, serde_json::json!({})).await;
        bridge.reload_embed().unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        // A key is part of the configuration: storing one rebuilds.
        let keyed: EmbedSettingsResult = serde_json::from_value(
            call(
                &client,
                EMBED_KEY_SET,
                serde_json::json!({ "key": "sk-secret" }),
            )
            .await,
        )
        .unwrap();
        assert!(keyed.key_set);
        assert_eq!(keyed.info.note, None);
        assert_eq!(builds.load(Ordering::SeqCst), 2);

        // Switched off on this device: no provider, and embed.embed says so.
        call(
            &client,
            EMBED_DEVICE_SET,
            serde_json::json!({ "slot": "remote", "enabled": false }),
        )
        .await;
        let info: EmbedInfoResult =
            serde_json::from_value(call(&client, EMBED_INFO, serde_json::json!({})).await).unwrap();
        assert!(!info.enabled);
        assert_eq!(info.note, Some(EmbedNote::DisabledOnDevice));
        assert!(
            client
                .call::<_, Value>(EMBED, serde_json::json!({ "texts": ["a"] }))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_model_synced_into_the_workgroup_root_is_picked_up_on_reload() {
        use sapphire_bridge_api::ModelSettings;

        let tmp = tempfile::tempdir().unwrap();
        let (bridge, _builds) = settings_bridge(&tmp, true);
        bridge.reload_embed().unwrap();
        assert!(!embed_info(&bridge).enabled);

        // What arrives from another device: the workgroup's file, written by its sync.
        let wg = bridge.workgroup().unwrap().unwrap();
        let models = ModelSettings {
            local: None,
            remote: Some(remote_model("from-another-device")),
        };
        std::fs::write(wg.embedding_toml(), toml::to_string(&models).unwrap()).unwrap();
        bridge.reload_embed().unwrap();

        let info = embed_info(&bridge);
        assert!(info.enabled);
        assert_eq!(info.model.unwrap().model, "from-another-device");
    }

    #[tokio::test]
    async fn without_avx2_a_local_only_workgroup_does_not_embed() {
        use sapphire_bridge_api::{EmbedNote, LocalModel, Slot, SlotModel};

        let tmp = tempfile::tempdir().unwrap();
        let (bridge, builds) = settings_bridge(&tmp, false);
        crate::embed_settings::set_model(
            &bridge.dir,
            EmbedModelSetParams {
                slot: Slot::Local,
                model: Some(SlotModel::Local(LocalModel::default())),
            },
        )
        .unwrap();
        bridge.reload_embed().unwrap();
        let info = embed_info(&bridge);
        assert!(!info.enabled);
        assert_eq!(info.note, Some(EmbedNote::NoAvx2));
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn status_and_settings_never_carry_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let (bridge, _builds) = settings_bridge(&tmp, true);
        let (client, _serving) = connect(&bridge).await;
        call(
            &client,
            EMBED_KEY_SET,
            serde_json::json!({ "key": "sk-very-secret" }),
        )
        .await;
        for method in [STATUS, EMBED_SETTINGS, EMBED_INFO] {
            let answer = call(&client, method, serde_json::json!({}))
                .await
                .to_string();
            assert!(!answer.contains("sk-very-secret"), "{method}: {answer}");
        }
    }
}
