//! The framework-provided half of an application's CLI.

use std::fmt::Write as _;
use std::path::PathBuf;

use sapphire_backend::protocol as proto;
use sapphire_bridge_api::{
    BridgeClient, DevicePrioritySetParams, DeviceRetireParams, InviteParams, JoinParams,
    WorkgroupCreateParams,
};
use sapphire_framework_service::{Environment, ServiceCommand, SystemManager};
use sapphire_ipc::{ClientInfo, Endpoint};

use crate::AppServer;
use crate::error::{Error, Result};

pub use sapphire_backend::protocol::{StatusReport, StatusRow};

/// The framework's commands, flattened into an application's CLI (spec decision 2).
///
/// An application composes these beside its own verbs by giving its subcommand enum a
/// `#[command(flatten)] Framework(FrameworkCommand)` variant, so every verb — the app's and
/// the framework's — sits at the same top level. The parse tests below pin that shape.
#[derive(Debug, Default, clap::Subcommand)]
pub enum FrameworkCommand {
    /// Run the server in this process until SIGTERM or SIGINT (the bare invocation).
    #[default]
    Serve,
    /// Report whether a server is running, and which version.
    Status,
    /// Install, remove or report this application's operating-system service.
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Create, list and map this application's workspaces.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Found, show or join the workgroup this device belongs to.
    #[command(subcommand)]
    Workgroup(WorkgroupCommand),
    /// List the workgroup's devices, invite one or retire one.
    #[command(subcommand)]
    Device(DeviceCommand),
    /// Show or change the embedding models (the workgroup's local and remote one), this
    /// device's switches, and its API key. Needs the running bridge.
    #[command(subcommand)]
    Embedding(sapphire_bridge_api::EmbeddingCommand),
    /// Add, list, retire, restore and rotate the workgroup's external devices — clients
    /// that reach this application with a key. `add` allows this application unless told
    /// otherwise. Needs the running bridge.
    #[command(subcommand)]
    ExternalDevice(sapphire_bridge_api::ExternalDeviceCommand),
}

impl FrameworkCommand {
    /// Carry out the command, returning the process exit code.
    ///
    /// The application matches its own variants first, then hands the framework's here;
    /// the app name and the service spec come from `server` itself.
    pub async fn dispatch(self, server: AppServer, version: &'static str) -> Result<i32> {
        match self {
            FrameworkCommand::Serve => {
                server.run().await?;
                Ok(0)
            }
            FrameworkCommand::Status => crate::command::status(&server, version).await,
            FrameworkCommand::Service(command) => {
                let spec = server.service_spec();
                command
                    .run(&spec, &Environment::detect(), &SystemManager)
                    .map_err(Error::from)
            }
            FrameworkCommand::Workspace(command) => {
                command.dispatch(server.app_name(), version).await
            }
            FrameworkCommand::Workgroup(command) => command.dispatch(version).await,
            FrameworkCommand::Device(command) => command.dispatch(version).await,
            FrameworkCommand::Embedding(command) => embedding(command, version).await,
            FrameworkCommand::ExternalDevice(command) => {
                external_device(command, server.app_name(), version).await
            }
        }
    }
}

/// The `workspace` subcommands (spec decisions 1/6/7).
///
/// `init` and `map`'s write go to the app's server over IPC, because the server owns the
/// marker directories, the registries and the sync ids; `list` asks the server for this
/// host's workspaces and then the bridge for the workgroup's ledger;
/// `map`'s selector resolution is the workgroup's word, so it goes to the bridge first.
#[derive(Debug, clap::Subcommand)]
pub enum WorkspaceCommand {
    /// Create this app's workspace home in the given directory.
    Init {
        /// Where the workspace root goes. Defaults to the current directory.
        dir: Option<PathBuf>,
        /// Also start syncing it, which publishes it into the workgroup ledger.
        #[arg(long)]
        sync: bool,
    },
    /// List this host's workspaces (from the server), then the workgroup's.
    List,
    /// Tie a local directory to a workspace the workgroup knows.
    Map {
        /// The workspace, by name or id, as the workgroup lists it.
        selector: String,
        /// The local directory to map it to. Defaults to the current one.
        dir: Option<PathBuf>,
    },
}

impl WorkspaceCommand {
    /// Carry out the command, returning the process exit code.
    ///
    /// `app` names the endpoint to open (`Endpoint::for_app`); `version` is this
    /// process's own, for the handshake.
    pub async fn dispatch(self, app: &'static str, version: &'static str) -> Result<i32> {
        match self {
            WorkspaceCommand::Init { dir, sync } => workspace_init(app, version, dir, sync).await,
            WorkspaceCommand::List => workspace_list(app, version).await,
            WorkspaceCommand::Map { selector, dir } => {
                workspace_map(app, version, &selector, dir).await
            }
        }
    }
}

/// The `workgroup` subcommands.
///
/// The workgroup's ledger is the bridge's business end to end, so these go to the bridge's
/// endpoint directly, including `create`, which goes through the running bridge.
#[derive(Debug, clap::Subcommand)]
pub enum WorkgroupCommand {
    /// Found a workgroup on this host.
    Create {
        /// The workgroup's name.
        name: String,
        /// This host's device name inside it.
        #[arg(long)]
        device_name: String,
    },
    /// Show the workgroup this host belongs to.
    List,
    /// Join the workgroup a ticket names.
    Join {
        /// The ticket the inviting device printed.
        ticket: String,
        /// The name this device will carry. Defaults to this host's name.
        #[arg(long)]
        device_name: Option<String>,
    },
}

impl WorkgroupCommand {
    /// Carry out the command, returning the process exit code.
    pub async fn dispatch(self, version: &'static str) -> Result<i32> {
        match self {
            WorkgroupCommand::Create { name, device_name } => {
                let client = connect_running(version).await?;
                let created = client
                    .workgroup_create(WorkgroupCreateParams { name, device_name })
                    .await?;
                println!(
                    "created workgroup {} ({}); this device is {}",
                    created.name, created.workgroup_id, created.device_id
                );
                Ok(0)
            }
            WorkgroupCommand::List => {
                let client = connect_running(version).await?;
                match client.status().await?.workgroup {
                    Some(workgroup) => {
                        println!("{} ({})", workgroup.name, workgroup.workgroup_id);
                        Ok(0)
                    }
                    None => {
                        println!("this host has not joined a workgroup");
                        Ok(1)
                    }
                }
            }
            WorkgroupCommand::Join {
                ticket,
                device_name,
            } => {
                let client = connect_running(version).await?;
                let joined = client
                    .join(JoinParams {
                        ticket,
                        device_name,
                    })
                    .await?;
                println!(
                    "joined workgroup {} ({}); this device is {}",
                    joined.workgroup_name, joined.workgroup_id, joined.device_id
                );
                Ok(0)
            }
        }
    }
}

/// The `device` subcommands.
///
/// The ledger's word for taking a device out is `retire` — a device id is written into
/// synced content and must keep resolving, so its record stays as a tombstone — and this
/// command goes through the running bridge to retire a device.
#[derive(Debug, clap::Subcommand)]
pub enum DeviceCommand {
    /// List the workgroup's devices, and which are reachable.
    List,
    /// Create an invite ticket for a device that is about to join.
    Invite {
        /// What the joining device will be called.
        #[arg(long)]
        name: String,
        /// How long the invite stays good, in seconds.
        #[arg(long)]
        ttl: Option<u64>,
        /// The workgroup to invite into, by name or id.
        #[arg(long)]
        workgroup: Option<String>,
    },
    /// Retire a device, so it may no longer connect.
    Retire {
        /// The device's name or id.
        selector: String,
    },
    /// Show or set a device's election priority (0-255; 0 = never primary or secondary).
    /// The election is non-preemptive: raising a priority does not move a role already held.
    Priority {
        /// The device's name or id.
        selector: String,
        /// The new priority. Omit to show the current one.
        priority: Option<u8>,
    },
}

impl DeviceCommand {
    /// Carry out the command, returning the process exit code.
    pub async fn dispatch(self, version: &'static str) -> Result<i32> {
        match self {
            DeviceCommand::List => {
                let client = connect_running(version).await?;
                let peers = client.peers().await?;
                if peers.peers.is_empty() {
                    println!("no devices");
                    return Ok(0);
                }
                for peer in peers.peers {
                    println!(
                        "{} {} p{}{}{}",
                        peer.name,
                        peer.device_id,
                        peer.priority,
                        peer.availability
                            .map(|a| format!(" a{a}"))
                            .unwrap_or_default(),
                        if peer.connected { " (online)" } else { "" }
                    );
                }
                Ok(0)
            }
            DeviceCommand::Invite {
                name,
                ttl,
                workgroup,
            } => {
                let client = connect_running(version).await?;
                let invite = client
                    .invite(InviteParams {
                        name,
                        ttl,
                        workgroup,
                    })
                    .await?;
                // The ticket on its own line, and nothing else on it: this is the one
                // output a user pipes to the joining device.
                println!("{}", invite.ticket);
                Ok(0)
            }
            DeviceCommand::Retire { selector } => {
                let client = connect_running(version).await?;
                let retired = client
                    .device_retire(DeviceRetireParams { selector })
                    .await?;
                println!("retired device {} ({})", retired.name, retired.device_id);
                Ok(0)
            }
            DeviceCommand::Priority { selector, priority } => {
                let client = connect_running(version).await?;
                match priority {
                    Some(priority) => {
                        let set = client
                            .device_priority_set(DevicePrioritySetParams { selector, priority })
                            .await?;
                        println!("{} ({}) priority {}", set.name, set.device_id, set.priority);
                    }
                    None => {
                        let peers = client.peers().await?;
                        match peers
                            .peers
                            .iter()
                            .find(|p| p.name == selector || p.device_id.to_string() == selector)
                        {
                            Some(p) => {
                                println!("{} ({}) priority {}", p.name, p.device_id, p.priority)
                            }
                            None => {
                                println!("no device {selector:?}");
                                return Ok(1);
                            }
                        }
                    }
                }
                Ok(0)
            }
        }
    }
}

/// Connect to the running bridge, or fail with the message the command layer prints.
///
/// One wrapper for every arm that talks to the bridge: the error's text is what the
/// `main` prints, so the absence case reads "no sapphire-bridge is running" without each
/// arm repeating it.
async fn connect_running(version: &str) -> Result<BridgeClient> {
    BridgeClient::connect_running("cli", version)
        .await
        .map_err(Error::from)
}

/// `embedding …`: always through the running bridge, whose files these are.
async fn embedding(command: sapphire_bridge_api::EmbeddingCommand, version: &str) -> Result<i32> {
    // Connect first: `key set` asks for the key, which is pointless with no bridge to take it.
    let client = connect_running(version).await?;
    let request = command.request()?;
    let report = client.embed_request(request).await?;
    for line in sapphire_bridge_api::describe(&report) {
        println!("{line}");
    }
    Ok(0)
}

/// `external-device …`: always through the running bridge, which owns the ledger.
async fn external_device(
    command: sapphire_bridge_api::ExternalDeviceCommand,
    app: &str,
    version: &str,
) -> Result<i32> {
    let client = connect_running(version).await?;
    let outcome = client
        .external_device_request(command.request(Some(app)))
        .await?;
    for line in sapphire_bridge_api::describe_external_outcome(&outcome) {
        println!("{line}");
    }
    Ok(0)
}

/// `workspace init`: the server does the creating, over IPC.
///
/// The server owns the marker, the registry and the sync ids, so a workspace created
/// here is one the server already knows and a GUI sharing the registry sees. Nothing is
/// listening → the one line and exit 1; nothing is ever started (Global Constraints).
/// `--sync` rides the server's existing `sync.enable` path on the same connection, and
/// its state is printed after, so an unjoined host's "no workgroup knows it yet" is
/// plain.
async fn workspace_init(
    app: &'static str,
    version: &'static str,
    dir: Option<PathBuf>,
    sync: bool,
) -> Result<i32> {
    let endpoint = Endpoint::for_app(app)?;
    let client_info = ClientInfo {
        kind: "cli".to_owned(),
        version: version.to_owned(),
        api: proto::API_VERSION,
        pid: std::process::id(),
    };
    let Some((client, _)) = sapphire_ipc::connect_or_absent(&endpoint, app, client_info).await?
    else {
        println!("no {app} server is running");
        return Ok(1);
    };

    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    let result: proto::WorkspaceInitResult = client
        .call(proto::WORKSPACE_INIT, proto::WorkspaceInitParams { dir })
        .await?;
    if result.created {
        println!("initialized {app} workspace in {}", result.root.display());
    } else {
        println!(
            "{} already exists (workspace {})",
            result.root.display(),
            result.workspace_id
        );
    }

    if sync {
        let enabled: proto::SyncEnableResult = client
            .call(
                proto::SYNC_ENABLE,
                proto::WsParams {
                    ws: result.root.clone(),
                },
            )
            .await?;
        // Never fails on account of the bridge: the workspace is synced either way, and
        // the status is the user's word for what the workgroup does not know yet.
        let status: proto::SyncStatusResult = client
            .call(
                proto::SYNC_STATUS,
                proto::WsParams {
                    ws: result.root.clone(),
                },
            )
            .await
            .unwrap_or(proto::SyncStatusResult {
                enabled: true,
                workspace_id: Some(enabled.workspace_id),
                ..proto::SyncStatusResult::not_synced()
            });
        println!(
            "syncing as {} ({} peer{})",
            enabled.workspace_id,
            status.peers,
            if status.peers == 1 { "" } else { "s" }
        );
    }
    Ok(0)
}

/// Render the server's `workspace.list` as the CLI prints it: `id root state`.
///
/// Public so the integration tests can capture it; the `workspace list` verb writes it to
/// standard output.
pub async fn render_workspace_list(client: &sapphire_ipc::Client, out: &mut String) -> Result<i32> {
    let list: proto::WorkspaceListResult = client
        .call(proto::WORKSPACE_LIST, serde_json::json!({}))
        .await?;
    if list.workspaces.is_empty() {
        writeln!(out, "no workspaces on this host").expect("writing to a String cannot fail");
    }
    for row in list.workspaces {
        let state = if !row.reachable {
            "unreachable"
        } else if row.sync.enabled {
            "synced"
        } else {
            "not synced"
        };
        let star = if matches!(row.sync.topology, proto::Topology::Star { .. }) {
            " (star)"
        } else {
            ""
        };
        let pending = match &row.sync.embedding {
            Some(e) if e.pending > 0 => format!(", embedding {} pending", e.pending),
            _ => String::new(),
        };
        writeln!(
            out,
            "{} {} {state}{star}{pending}",
            row.id,
            row.root.display()
        )
        .expect("writing to a String cannot fail");
    }
    Ok(0)
}

/// `workspace list`: this host's workspaces from the server, then the workgroup's ledger.
///
/// The server owns the host registry, so the first half is its `workspace.list`; nothing
/// listening → the one line and exit 1, like `init`. The workgroup's ledger is the bridge's
/// to answer, and the bridge being down is not the server half's failure: the ledger's
/// absence is one line, and the exit is 0.
async fn workspace_list(app: &'static str, version: &'static str) -> Result<i32> {
    let endpoint = Endpoint::for_app(app)?;
    let client_info = ClientInfo {
        kind: "cli".to_owned(),
        version: version.to_owned(),
        api: proto::API_VERSION,
        pid: std::process::id(),
    };
    let Some((client, _)) = sapphire_ipc::connect_or_absent(&endpoint, app, client_info).await?
    else {
        println!("no {app} server is running");
        return Ok(1);
    };
    let mut out = String::new();
    let code = render_workspace_list(&client, &mut out).await?;
    for line in out.lines() {
        println!("{line}");
    }
    if code != 0 {
        return Ok(code);
    }

    let Some(client) = BridgeClient::connect_running("cli", version).await.ok() else {
        println!("no sapphire-bridge is running");
        return Ok(0);
    };
    let workspaces = client.workspaces().await.map_err(Error::from)?;
    if !workspaces.workspaces.is_empty() {
        println!();
        for workspace in workspaces.workspaces {
            println!(
                "{} {} {}",
                workspace.name, workspace.workspace_id, workspace.app_name
            );
        }
    }
    Ok(0)
}

/// `workspace map`: the bridge resolves the selector, the server takes the write.
///
/// The resolution is the workgroup's word (which workspace, by what id), the write is the
/// server's (`sync.map` places the id and starts the replica). The bridge is required —
/// without it there is nothing to resolve against, so exit 1 — and so is the server,
/// because the write and the replica are its.
async fn workspace_map(
    app: &'static str,
    version: &'static str,
    selector: &str,
    dir: Option<PathBuf>,
) -> Result<i32> {
    let Some(client) = BridgeClient::connect_running("cli", version).await.ok() else {
        println!("no sapphire-bridge is running");
        return Ok(1);
    };
    let workspaces = client.workspaces().await.map_err(Error::from)?;
    let wanted = workspaces
        .workspaces
        .iter()
        // A name is the user-facing handle; the id is the exact one.
        .find(|w| w.name == selector || w.workspace_id.to_string() == selector)
        .ok_or_else(|| Error::UnknownWorkspaceName(selector.to_owned()))?;

    let endpoint = Endpoint::for_app(app)?;
    let client_info = ClientInfo {
        kind: "cli".to_owned(),
        version: version.to_owned(),
        api: proto::API_VERSION,
        pid: std::process::id(),
    };
    let Some((client, _)) = sapphire_ipc::connect_or_absent(&endpoint, app, client_info).await?
    else {
        println!("no {app} server is running");
        return Ok(1);
    };

    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    let mapped: proto::SyncEnableResult = client
        .call(
            proto::SYNC_MAP,
            proto::SyncMapParams {
                workspace: wanted.workspace_id.to_string(),
                dir,
            },
        )
        .await?;
    println!("mapped {} as {}", wanted.name, mapped.workspace_id);
    Ok(0)
}

/// The status command's face: render and return the exit code, going to the process's
/// standard output.
async fn status(server: &AppServer, version: &str) -> Result<i32> {
    let endpoint = Endpoint::for_app(server.app_name())?;
    run_status(&endpoint, server.app_name(), version).await
}

/// Probe, handshake and call `SERVER_INFO` in one go, or report absence.
///
/// Every step the CLI needs — liveness and the API gate — is inside
/// [`connect_or_absent`], so the command is that call plus one `SERVER_INFO` round trip.
/// The API gate doubles as the liveness check here: a live server of another API version
/// is `ApiVersionMismatch`, which names both API versions and advises upgrading and
/// restarting the service. The report goes through [`run_status_into`] so tests can
/// capture it.
async fn run_status(endpoint: &Endpoint, app: &str, version: &str) -> Result<i32> {
    let mut out = String::new();
    let code = run_status_into(endpoint, app, version, &mut out).await?;
    for line in out.lines() {
        println!("{line}");
    }
    Ok(code)
}

/// [`run_status`], writing the rendered lines into `out` instead of the process's
/// standard output.
async fn run_status_into(
    endpoint: &Endpoint,
    app: &str,
    version: &str,
    out: &mut String,
) -> Result<i32> {
    let client_info = ClientInfo {
        kind: "cli".to_owned(),
        version: version.to_owned(),
        api: proto::API_VERSION,
        pid: std::process::id(),
    };
    let Some((client, _)) = sapphire_ipc::connect_or_absent(endpoint, app, client_info).await?
    else {
        // Nothing is listening: the framework's half is only the one line, and the
        // application's rows are skipped (there is no server to have configured them).
        writeln!(out, "no {app} server is running").expect("writing to a String cannot fail");
        return Ok(1);
    };
    let report: StatusReport = client
        .call(
            sapphire_backend::protocol::SERVER_INFO,
            serde_json::json!({}),
        )
        .await?;
    writeln!(out, "running: {}", report.running).expect("writing to a String cannot fail");
    if let Some(version) = &report.version {
        writeln!(out, "version: {version}").expect("writing to a String cannot fail");
    }
    if let Some(pid) = report.pid {
        writeln!(out, "pid: {pid}").expect("writing to a String cannot fail");
    }
    if let Some(managed_by) = &report.managed_by {
        writeln!(out, "managed_by: {managed_by:?}").expect("writing to a String cannot fail");
    }
    for row in &report.app {
        writeln!(out, "{}: {}", row.name, row.value).expect("writing to a String cannot fail");
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// The composition every app binary uses: an app-owned subcommand enum whose framework
    /// verbs ride in on a flattened variant (spec decision 2).
    ///
    /// clap 4 rejects `#[command(flatten)]` on a struct field beside a
    /// `#[command(subcommand)]` field — two subcommand enums cannot share one level, and a
    /// flattened field must implement `clap::Args`, which the `Subcommand` derive does not
    /// provide. The plan's Risks section names this probe as the decision, and the flat
    /// fallback is what it decided: one subcommand enum, every verb at the top level.
    #[derive(Parser)]
    struct Probe {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Debug, clap::Subcommand)]
    enum Command {
        /// An app-specific verb that must parse beside the framework's.
        Greet,
        #[command(flatten)]
        Framework(FrameworkCommand),
    }

    #[test]
    fn app_and_framework_commands_parse_side_by_side() {
        let probe = Probe::try_parse_from(["app", "greet"]).unwrap();
        assert!(matches!(probe.command, Command::Greet));
        let probe = Probe::try_parse_from(["app", "serve"]).unwrap();
        assert!(matches!(
            probe.command,
            Command::Framework(FrameworkCommand::Serve)
        ));
        let probe = Probe::try_parse_from(["app", "status"]).unwrap();
        assert!(matches!(
            probe.command,
            Command::Framework(FrameworkCommand::Status)
        ));
    }

    #[test]
    fn the_service_subcommands_parse() {
        for args in [
            vec!["app", "service", "install"],
            vec!["app", "service", "uninstall"],
            vec!["app", "service", "status"],
        ] {
            assert!(Probe::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn the_workspace_and_bridge_commands_parse() {
        for args in [
            vec!["app", "workspace", "init"],
            vec!["app", "workspace", "init", "papers"],
            vec!["app", "workspace", "init", "--sync"],
            vec!["app", "workspace", "list"],
            vec!["app", "workspace", "map", "papers", "papers-remote"],
            vec![
                "app",
                "workgroup",
                "create",
                "--device-name",
                "laptop",
                "home",
            ],
            vec!["app", "workgroup", "list"],
            vec![
                "app",
                "workgroup",
                "join",
                "--device-name",
                "laptop",
                "TICKET",
            ],
            vec!["app", "device", "list"],
            vec!["app", "device", "invite", "--name", "phone"],
            vec!["app", "device", "retire", "phone"],
        ] {
            assert!(Probe::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn bare_invocation_defaults_to_serve() {
        // <app> with no subcommand at all means serve (spec decision 1). An app CLI
        // achieves it by `Option<Command>` and handing the framework's default here; this
        // probe pins the framework half.
        #[derive(Parser)]
        struct Bare {
            #[command(subcommand)]
            command: Option<Command>,
        }
        let bare = Bare::try_parse_from(["app"]).unwrap();
        assert!(bare.command.is_none());
        assert!(matches!(
            FrameworkCommand::default(),
            FrameworkCommand::Serve
        ));
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use sapphire_workspace::AppContext;

    static CTX: AppContext = AppContext::new("sapphire-statustest");

    /// Wait until something is listening on `endpoint`.
    ///
    /// `tokio::spawn(server.run())` only schedules the server; without this wait a fast
    /// `run_status` probe can run before `run` has bound the socket, and with nothing
    /// listening that single unlucky probe is the whole test failing.
    async fn wait_until_listening(endpoint: &Endpoint) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !sapphire_ipc::probe(endpoint).await.unwrap() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the server never started listening"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn status_reports_no_server_when_none_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = sapphire_ipc::Endpoint::in_dir("status-test", tmp.path().to_path_buf());
        let code = run_status(&endpoint, "status-test", "0.0.0").await.unwrap();
        assert_eq!(code, 1, "no server is a non-zero exit");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_shows_the_extension_rows_of_a_running_server() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = sapphire_ipc::Endpoint::in_dir("status-test", tmp.path().to_path_buf());
        let server = AppServer::new(&CTX, "0.0.0")
            .endpoint(endpoint.clone())
            .status_rows(std::sync::Arc::new(|| {
                vec![StatusRow {
                    name: "sync".into(),
                    value: "on".into(),
                }]
            }));
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;

        // The report goes through the sink face so the rendering is asserted, not just
        // the exit code.
        let mut out = String::new();
        let code = run_status_into(&endpoint, CTX.app_name, "0.0.0", &mut out)
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("running: true"), "output was: {out}");
        assert!(out.contains("sync: on"), "output was: {out}");

        let (client, _) = sapphire_ipc::connect_or_absent(
            &endpoint,
            CTX.app_name,
            sapphire_ipc::ClientInfo {
                kind: "test".into(),
                version: "0.0.0".into(),
                api: sapphire_backend::protocol::API_VERSION,
                pid: std::process::id(),
            },
        )
        .await
        .unwrap()
        .expect("the server is listening");
        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();
        handle.await.unwrap().unwrap();
    }
}

#[cfg(test)]
mod service_spec_tests {
    use super::*;
    use sapphire_workspace::AppContext;

    static CTX: AppContext = AppContext::new("sapphire-servicetest");

    #[test]
    fn the_generated_spec_runs_the_server_not_the_cli() {
        // A service manager starts the executable directly, and the executable's bare
        // invocation is `serve`: the unit's argv must be exactly that, so the server that
        // starts has no CLI verbs on its command line to misparse.
        let spec = AppServer::new(&CTX, "0.0.0").service_spec();
        assert_eq!(spec.args, vec!["serve".to_owned()]);
    }

    #[test]
    fn the_generated_spec_names_this_application() {
        let spec = AppServer::new(&CTX, "0.0.0").service_spec();
        assert_eq!(spec.app_name, CTX.app_name);
    }

    #[test]
    fn the_generated_spec_describes_the_server() {
        let spec = AppServer::new(&CTX, "0.0.0").service_spec();
        assert!(
            spec.description.contains(CTX.app_name),
            "the unit's description should name the application: {:?}",
            spec.description
        );
        assert!(spec.description.contains("0.0.0"));
    }
}
