//! What the client layer hands the views, and what they hand back.

use std::path::PathBuf;
use std::time::Duration;

use grain_id::GrainId;
use sapphire_backend::protocol::{StatusReport, WorkspaceListEntry};
use sapphire_bridge_api::{BRIDGE_NAME, PeerInfo, StatusResult, WorkgroupWorkspaceInfo};
use sapphire_ipc::Endpoint;

/// Which application this GUI belongs to.
#[derive(Clone, Copy, Debug)]
pub struct AppIdentity {
    /// The app name, which names its server's endpoint and its marker (`.{app_name}`).
    pub app_name: &'static str,
    /// This GUI's version, for the handshake.
    pub version: &'static str,
}

/// Where the two processes listen.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// The bridge's control plane.
    pub bridge: Endpoint,
    /// The app server.
    pub app: Endpoint,
}

impl Endpoints {
    /// The built-in endpoints in this user's runtime directory.
    pub fn default_for(app_name: &str) -> sapphire_ipc::Result<Endpoints> {
        let dir = sapphire_ipc::runtime_dir()?;
        Ok(Endpoints {
            bridge: Endpoint::in_dir(BRIDGE_NAME, dir.clone()),
            app: Endpoint::in_dir(app_name, dir),
        })
    }
}

/// The executables whose `service install` verb registers each service.
#[derive(Clone, Debug)]
pub struct ServiceExes {
    /// `sapphire-bridge`.
    pub bridge: PathBuf,
    /// The application's own CLI.
    pub app: PathBuf,
}

impl ServiceExes {
    /// The binaries next to the running executable — how the apps are shipped.
    pub fn siblings(app_name: &str) -> std::io::Result<ServiceExes> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().map(PathBuf::from).unwrap_or_default();
        let suffix = std::env::consts::EXE_SUFFIX;
        Ok(ServiceExes {
            bridge: dir.join(format!("sapphire-bridge{suffix}")),
            app: dir.join(format!("{app_name}{suffix}")),
        })
    }
}

/// Everything [`FrameworkClient::spawn`](super::FrameworkClient::spawn) needs.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Which application.
    pub app: AppIdentity,
    /// Where to connect.
    pub endpoints: Endpoints,
    /// What to run for "install & start".
    pub service_exes: ServiceExes,
    /// How often to refresh the snapshot.
    pub refresh: Duration,
    /// How long one process may take to answer a refresh (connect, handshake and calls)
    /// before it reads as not answering.
    pub fetch_timeout: Duration,
    /// How long a command may take; a join, which dials peers, gets three times this.
    pub command_timeout: Duration,
}

impl ClientConfig {
    /// The defaults: built-in endpoints, sibling executables, a 2 s refresh.
    pub fn new(app: AppIdentity) -> std::io::Result<ClientConfig> {
        Ok(ClientConfig {
            endpoints: Endpoints::default_for(app.app_name).map_err(std::io::Error::other)?,
            service_exes: ServiceExes::siblings(app.app_name)?,
            refresh: Duration::from_secs(2),
            fetch_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(30),
            app,
        })
    }
}

/// One process, as last seen.
#[derive(Clone, Debug)]
pub enum Conn<T> {
    /// Nothing is listening.
    Absent,
    /// It answers, but speaks another API version. The message says which to update.
    Incompatible(String),
    /// It answered with an error, or the connection failed mid-call.
    Error(String),
    /// It answered.
    Up(T),
}

impl<T> Conn<T> {
    /// The state, when the process answered.
    pub fn up(&self) -> Option<&T> {
        match self {
            Conn::Up(t) => Some(t),
            _ => None,
        }
    }
}

/// What the bridge said.
#[derive(Clone, Debug)]
pub struct BridgeState {
    /// `bridge.status`.
    pub status: StatusResult,
    /// `bridge.peers` (empty without a workgroup).
    pub peers: Vec<PeerInfo>,
    /// `bridge.workspaces` (empty without a workgroup).
    pub ledger: Vec<WorkgroupWorkspaceInfo>,
}

/// What the app server said.
#[derive(Clone, Debug)]
pub struct ServerState {
    /// `server.info`.
    pub info: StatusReport,
    /// `workspace.list`.
    pub workspaces: Vec<WorkspaceListEntry>,
}

/// Both processes, as of the last refresh.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The bridge.
    pub bridge: Conn<BridgeState>,
    /// The app server.
    pub server: Conn<ServerState>,
    /// `false` until the first refresh has finished.
    pub fetched: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            bridge: Conn::Absent,
            server: Conn::Absent,
            fetched: false,
        }
    }
}

/// Which service an install targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceTarget {
    /// `sapphire-bridge`.
    Bridge,
    /// The application's server.
    App,
}

/// Something the user asked for.
#[derive(Clone, Debug)]
pub enum Command {
    /// Found a workgroup.
    WorkgroupCreate {
        /// The workgroup's name.
        name: String,
        /// This device's name in it.
        device_name: String,
    },
    /// Join with a ticket.
    WorkgroupJoin {
        /// The invite ticket.
        ticket: String,
        /// This device's name, or the bridge's default.
        device_name: Option<String>,
    },
    /// Issue an invite ticket.
    DeviceInvite {
        /// The name the invited device will carry.
        name: String,
        /// How long the ticket stays valid, in seconds.
        ttl_secs: Option<u64>,
    },
    /// Retire a device.
    DeviceRetire {
        /// The device's name or id.
        selector: String,
    },
    /// Create (or adopt) a workspace in `dir`, optionally syncing it.
    WorkspaceInit {
        /// The workspace directory.
        dir: PathBuf,
        /// Enable sync right after.
        sync: bool,
    },
    /// Start syncing.
    SyncEnable {
        /// The workspace root.
        root: PathBuf,
    },
    /// Stop syncing.
    SyncDisable {
        /// The workspace root.
        root: PathBuf,
    },
    /// Bring a workgroup workspace to `dir`, creating the folder and initialising it as a
    /// workspace first if needed.
    WorkspaceMap {
        /// The workgroup workspace to map.
        workspace_id: GrainId,
        /// Where it goes on this host.
        dir: PathBuf,
    },
    /// Drop from this host's list.
    WorkspaceForget {
        /// The host registry id.
        id: String,
    },
    /// Run the target's `service install`.
    ServiceInstall(ServiceTarget),
}

/// What a finished command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandOutput {
    /// Nothing to show beyond success.
    Done,
    /// An invite ticket, to show once.
    Ticket(String),
}

/// Names one sent command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommandId(pub u64);

/// How a sent command ended.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Which command.
    pub id: CommandId,
    /// Its result; the error is the message to show.
    pub result: Result<CommandOutput, String>,
}
