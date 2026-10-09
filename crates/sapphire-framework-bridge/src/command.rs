//! The `sapphire-bridge` command line.
//!
//! The same flat vocabulary every sapphire CLI speaks (2026-09-24 spec decisions 1/2), with
//! this bridge's own `log` beside it. Running it with no subcommand starts the bridge; every
//! other subcommand is a one-shot command against a running one. Two of them work directly on
//! the bridge directory instead — [`WorkgroupCommand::Create`], because there is nothing to
//! ask about a workgroup that does not exist yet, and [`DeviceCommand::Retire`], because the
//! control plane has no method for it. Both then see their effect immediately: the ledger is
//! re-read on every authorization.

// The transport is only assembled behind the `node` feature; the imports that build it are
// gated with it so a build without the feature stays warning-free.
#[cfg(feature = "node")]
use std::sync::Arc;

use std::io::Write as _;

use sapphire_bridge_api::{
    BRIDGE_NAME, BridgeClient, EmbedInfoResult, InviteParams, JoinParams, PeerInfo, StatusResult,
};
use sapphire_framework_service::{Environment, ServiceCommand, ServiceSpec, SystemManager};
use sapphire_ipc::Endpoint;

#[cfg(feature = "node")]
use crate::NetConfig;
use crate::error::{Error, Result};
use crate::status::StatusFile;
use crate::workgroup::Workgroup;
use crate::{Bridge, BridgeDir, InstanceLock};

/// The bridge daemon's commands.
///
/// The same flat vocabulary every sapphire CLI speaks (2026-09-24 spec decisions
/// 1/2), with this bridge's own `log` beside it. The bridge keeps its own enum
/// rather than reusing the app side's `FrameworkCommand`: that dispatcher takes an
/// `AppServer`, and this crate is the switchboard *above* app servers — it must not
/// depend on the `-server` crate. Two verbs work directly on the bridge directory
/// instead of over IPC: `workgroup create`, because there is nothing to ask about a
/// workgroup that does not exist yet, and `device retire`, because the control
/// plane has no method for it. Both see their effect at once: the ledger is
/// re-read on every authorization.
#[derive(Debug, Default, clap::Subcommand)]
pub enum BridgeCommand {
    /// Run the bridge in this process (the bare invocation).
    #[default]
    Serve,
    /// Report what the bridge knows — live, or from the last snapshot it wrote.
    Status,
    /// Show the bridge's log. Bridge-specific: only this process writes one.
    Log {
        /// Keep printing as the log grows, as `tail -f` does.
        #[arg(long)]
        follow: bool,
        /// How many lines of the log's end to show.
        #[arg(long, default_value = "20")]
        lines: usize,
    },
    /// Install, remove or report this host's bridge service.
    #[command(subcommand)]
    Service(ServiceCommand),
    /// What the workgroup contains — read-only.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// This host's workgroup.
    #[command(subcommand)]
    Workgroup(WorkgroupCommand),
    /// The workgroup's devices.
    #[command(subcommand)]
    Device(DeviceCommand),
}

/// The service this binary installs.
///
/// The unit runs the bare binary with `serve`, so a service manager starts a bridge and
/// nothing else. The description names `version`, so whoever reads the installed unit can
/// tell which build it starts without inspecting the binary; the frame is what an app's own
/// spec says too.
pub fn bridge_service_spec(version: &str) -> ServiceSpec {
    ServiceSpec {
        app_name: "sapphire-bridge",
        description: format!("Sapphire bridge daemon {version}"),
        args: vec!["serve".to_owned()],
        post_install: None,
    }
}

/// `device` subcommands.
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
    ///
    /// The record stays as a tombstone: a device id is written into synced content and must
    /// keep resolving.
    Retire {
        /// The device's name or id.
        selector: String,
    },
}

/// `workgroup` subcommands.
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

/// `workspace` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum WorkspaceCommand {
    /// List the workspaces this host serves.
    List,
}

impl BridgeCommand {
    /// Carry out the command, returning the process exit code.
    pub async fn dispatch(self, version: &'static str) -> Result<i32> {
        self.dispatch_with(version, None).await
    }

    /// As [`BridgeCommand::dispatch`], with a factory for the embedding provider that
    /// `serve` installs once the bridge directory is open.
    pub async fn dispatch_with(
        self,
        version: &'static str,
        embed: Option<crate::EmbedFactory>,
    ) -> Result<i32> {
        match self {
            BridgeCommand::Serve => run(version, embed).await,
            BridgeCommand::Status => status(version).await,
            BridgeCommand::Log { follow, lines } => log_command(follow, lines),
            // The service manager's own words, not the bridge's: `Environment::detect` reads
            // this machine once, so the deciding is a function of a value the test can build.
            BridgeCommand::Service(command) => {
                let spec = bridge_service_spec(version);
                command
                    .run(&spec, &Environment::detect(), &SystemManager)
                    .map_err(Error::from)
            }
            BridgeCommand::Workspace(command) => match command {
                WorkspaceCommand::List => workspace_list(version).await,
            },
            BridgeCommand::Workgroup(command) => match command {
                WorkgroupCommand::Create { name, device_name } => {
                    workgroup_create(version, &name, &device_name).await
                }
                WorkgroupCommand::List => workgroup_list(),
                WorkgroupCommand::Join {
                    ticket,
                    device_name,
                } => workgroup_join(version, ticket, device_name).await,
            },
            BridgeCommand::Device(command) => match command {
                DeviceCommand::List => device_list(version).await,
                DeviceCommand::Invite {
                    name,
                    ttl,
                    workgroup,
                } => device_invite(version, name, ttl, workgroup).await,
                DeviceCommand::Retire { selector } => device_retire(version, &selector).await,
            },
        }
    }
}

// ── run ─────────────────────────────────────────────────────────────────────

/// Run the bridge in this process — what the bare invocation, and `serve`, mean.
///
/// The bare invocation is the same thing as `serve`; the enum's default is [`BridgeCommand::Serve`].
/// A second start is a normal thing to do by accident, so "already running" is a reported
/// outcome, not an error.
async fn run(version: &'static str, embed: Option<crate::EmbedFactory>) -> Result<i32> {
    let dir = BridgeDir::open()?;
    // Held for as long as the bridge runs; dropping it frees the next start. The lock is
    // taken before anything reads `net.toml` or `node.key`, so a directory of a newer format
    // stops us here rather than after a new identity has been minted (see `BridgeDir::at`).
    let _lock = match InstanceLock::acquire(&dir) {
        Ok(lock) => lock,
        // Not an error: starting the bridge twice is a normal thing to do by accident, and
        // the answer is the pid of the one that is already there.
        Err(Error::AlreadyRunning(pid)) => {
            println!("the bridge is already running (pid {pid})");
            return Ok(1);
        }
        Err(err) => return Err(err),
    };

    // One writer, guaranteed by the lock above: the log file is this bridge's alone for
    // as long as it runs, and the guard keeps the writer thread alive until the run ends.
    let _log = crate::logging::install(&dir)?;
    // The record's first line names the run, so a log read across restarts shows where one
    // bridge's story ended and the next began.
    tracing::info!(
        target: crate::logging::BRIDGE_TARGET,
        "sapphire-bridge {version} starting (pid {})",
        std::process::id()
    );
    let provider = embed.and_then(|factory| factory(&dir));
    let mut bridge = build_bridge(dir, version).await?;
    if let Some(provider) = provider {
        bridge = bridge.embed_provider(provider);
    }
    bridge.run().await?;
    Ok(0)
}

/// The bridge this host should run: the iroh transport, over the directory's configuration.
#[cfg(feature = "node")]
async fn build_bridge(dir: BridgeDir, version: &'static str) -> Result<Bridge> {
    // Read once, and hand the same configuration to the bridge: it would otherwise read
    // `net.toml` again when it starts its loops, and the two reads could disagree.
    let net = NetConfig::load(&dir.net_toml())?;
    // The workgroup's published `net.toml` is read inside `new_with_relays`, so a
    // publish that landed between the two reads is not split across them either.
    let transport = Arc::new(
        crate::iroh::IrohTransport::new_with_relays(
            &dir.node_key(),
            &net,
            crate::workgroup::Workgroup::open(&dir)?.as_ref(),
        )
        .await?,
    );
    Ok(Bridge::new(dir, transport, version)?.net(net))
}

/// Without the `node` feature there is no transport to reach other devices with.
#[cfg(not(feature = "node"))]
async fn build_bridge(_dir: BridgeDir, _version: &'static str) -> Result<Bridge> {
    Err(Error::Config(
        "this build has no peer transport; rebuild sapphire-bridge with the `node` feature"
            .to_owned(),
    ))
}

// ── one-shot commands ───────────────────────────────────────────────────────

/// Connect to the running bridge, or `None` when no sapphire-bridge is running.
///
/// Never starts one: asking a question about the bridge must not bring a daemon up.
async fn connect(version: &str) -> Result<Option<BridgeClient>> {
    let endpoint = Endpoint::in_dir(BRIDGE_NAME, sapphire_ipc::runtime_dir()?);
    if !sapphire_ipc::probe(&endpoint).await? {
        return Ok(None);
    }
    Ok(Some(BridgeClient::connect("cli", version).await?))
}

/// Report what the running bridge knows about itself.
///
/// A live bridge answers on the control plane, where every fact is current. A bridge that
/// has stopped still has a story worth telling — which app servers were connected, what the
/// workgroup looked like — so when nothing answers, the last snapshot of `status.json` is
/// reported instead, printed the same way so nobody has to learn two formats.
async fn status(version: &str) -> Result<i32> {
    let report = match connect(version).await? {
        Some(client) => Some(StatusReport::live(
            client.status().await?,
            client.peers().await?.peers,
        )?),
        None => read_status_report()?,
    };
    let Some(report) = report else {
        println!("no sapphire-bridge is running");
        return Ok(1);
    };

    println!(
        "sapphire-bridge {} (node {}, {} workspace(s)){}",
        report.status.version,
        report.status.node_id,
        report.status.routes.len(),
        if report.stale {
            " — last seen before the bridge stopped"
        } else {
            ""
        }
    );
    match report.status.workgroup {
        Some(workgroup) => println!(
            "workgroup {} ({}), {} device(s)",
            workgroup.name, workgroup.workgroup_id, workgroup.devices
        ),
        None => println!("no workgroup"),
    }
    // One embedding line, whatever the source: disabled, or the model and dimension and
    // whether it is in memory. A snapshot from before the line existed has nothing to say,
    // and prints nothing rather than claiming the host does not embed.
    if let Some(embedding) = &report.status.embedding {
        println!("{}", embedding_line(embedding));
    }
    for peer in report.peers {
        println!(
            "  {} {} {}{}",
            peer.name,
            peer.device_id,
            if peer.node_id.is_empty() {
                "-".to_owned()
            } else {
                peer.node_id
            },
            if peer.connected { " (online)" } else { "" }
        );
    }
    for route in report.status.routes {
        println!(
            "  {} {} {}{}",
            route.workspace_id,
            route.app_name,
            route.root.display(),
            if route.owner_online { " (online)" } else { "" }
        );
    }
    Ok(0)
}

/// The one `bridge status` embedding line: disabled, or the model, its dimension and
/// whether it is loaded.
fn embedding_line(embedding: &EmbedInfoResult) -> String {
    if !embedding.enabled {
        return "embedding disabled".to_owned();
    }
    match &embedding.model {
        Some(model) => format!(
            "embedding {} ({} dimension(s)), {}",
            model.model,
            model.dimension,
            if embedding.loaded {
                "loaded"
            } else {
                "not loaded"
            }
        ),
        // An enabled embedding with no model is a provider that is up but has nothing to
        // say; report it as such rather than inventing a model.
        None => format!(
            "embedding enabled, {}",
            if embedding.loaded {
                "loaded"
            } else {
                "not loaded"
            }
        ),
    }
}

/// What `bridge status` prints: the control plane's answer, or the last snapshot.
struct StatusReport {
    status: StatusResult,
    peers: Vec<PeerInfo>,
    /// Whether this came from the file a stopped bridge left behind.
    stale: bool,
}

impl StatusReport {
    /// A live answer: the control plane's own status and device list.
    fn live(status: StatusResult, peers: Vec<PeerInfo>) -> Result<StatusReport> {
        Ok(StatusReport {
            status,
            peers,
            stale: false,
        })
    }
}

/// The last snapshot `status.json` holds, as a report.
///
/// `None` when the bridge has never run here; an unreadable snapshot is an error rather
/// than something to guess at.
fn read_status_report() -> Result<Option<StatusReport>> {
    let dir = BridgeDir::open()?;
    match StatusFile::load(&dir.status_json())? {
        None => Ok(None),
        Some(snapshot) => Ok(Some(StatusReport {
            peers: snapshot
                .peers
                .iter()
                .map(|p| PeerInfo {
                    device_id: p.device_id,
                    name: p.name.clone(),
                    node_id: p.node_id.clone(),
                    connected: p.connected,
                })
                .collect(),
            status: StatusResult {
                version: snapshot.version,
                node_id: snapshot.node_id,
                workgroup: snapshot.workgroup,
                routes: snapshot.routes,
                // The last snapshot recorded what the bridge was embedding; a snapshot
                // written before the line existed simply has none.
                embedding: snapshot.embedding,
            },
            stale: true,
        })),
    }
}

/// Show the end of the bridge's log, and with `--follow` keep showing it.
///
/// Works directly on the bridge directory: the log is a file this user already owns, and
/// asking to read it must not bring a daemon up. A bridge that has never run here has no
/// log, which is reported rather than failed.
fn log_command(follow: bool, lines: usize) -> Result<i32> {
    let dir = BridgeDir::open()?;
    if !follow {
        let lines = crate::logging::tail(&dir, lines)?;
        if lines.is_empty() {
            println!("the bridge has not written a log yet");
            return Ok(1);
        }
        let mut out = std::io::stdout().lock();
        for line in &lines {
            writeln!(out, "{line}").map_err(Error::Io)?;
        }
        return Ok(0);
    }
    // Following never returns on its own; the process is interrupted instead, exactly as
    // `tail -f` ends.
    let mut out = std::io::stdout().lock();
    crate::logging::follow(&dir, lines, &mut out, || false)?;
    Ok(0)
}

/// List the workgroup's devices, as the running bridge sees them.
async fn device_list(version: &str) -> Result<i32> {
    let Some(client) = connect(version).await? else {
        println!("no sapphire-bridge is running");
        return Ok(1);
    };
    let peers = client.peers().await?;
    if peers.peers.is_empty() {
        println!("no devices");
        return Ok(0);
    }
    for peer in peers.peers {
        println!(
            "{} {} {}{}",
            peer.name,
            peer.device_id,
            if peer.node_id.is_empty() {
                "-".to_owned()
            } else {
                peer.node_id
            },
            if peer.connected { " (online)" } else { "" }
        );
    }
    Ok(0)
}

/// Create an invite ticket, printed alone on its line so it can be piped.
///
/// The running bridge composes it: the ticket names the address a joiner must dial, and only
/// the bridge holds the bound endpoint. Asking must not start a bridge, so a host with none
/// running is reported, not started.
async fn device_invite(
    version: &str,
    name: String,
    ttl: Option<u64>,
    workgroup: Option<String>,
) -> Result<i32> {
    let Some(client) = connect(version).await? else {
        println!("no sapphire-bridge is running");
        return Ok(1);
    };
    let invite = client
        .invite(InviteParams {
            name,
            ttl,
            workgroup,
        })
        .await
        .map_err(Error::Ipc)?;
    // The ticket on its own line, and nothing else on that line: this is the one output a
    // user is meant to pipe into the joining device.
    println!("{}", invite.ticket);
    Ok(0)
}

/// Join the workgroup a ticket names.
///
/// The running bridge does the pairing: it holds the endpoint the exchange runs over, and it
/// is the process that must go on to serve the workgroup this host is joining. Asking must
/// not start a bridge, so a host with none running is reported, not started.
async fn workgroup_join(version: &str, ticket: String, device_name: Option<String>) -> Result<i32> {
    let Some(client) = connect(version).await? else {
        println!("no sapphire-bridge is running");
        return Ok(1);
    };
    let joined = client
        .join(JoinParams {
            ticket,
            device_name,
        })
        .await
        .map_err(Error::Ipc)?;
    println!(
        "joined workgroup {} ({}); this device is {}",
        joined.workgroup_name, joined.workgroup_id, joined.device_id
    );
    Ok(0)
}

/// Retire a device in the ledger.
///
/// A running bridge does it over the control plane, so one process writes the ledger; a
/// stopped one is edited directly, as before. Either way this host's own device is refused.
async fn device_retire(version: &str, selector: &str) -> Result<i32> {
    if let Some(client) = connect(version).await? {
        let retired = client
            .device_retire(sapphire_bridge_api::DeviceRetireParams {
                selector: selector.to_owned(),
            })
            .await?;
        println!("retired device {} ({})", retired.name, retired.device_id);
        return Ok(0);
    }
    let dir = BridgeDir::open()?;
    let Some(workgroup) = Workgroup::open(&dir)? else {
        println!("this host has not joined a workgroup");
        return Ok(1);
    };
    let device = workgroup.retire_device(selector, &this_node_id(&dir)?)?;
    println!("retired device {} ({})", device.name, device.id);
    Ok(0)
}

/// Found a workgroup, recording this host as its first device.
///
/// A running bridge does it over the control plane and serves the workgroup at once; a
/// stopped one is written directly, as before.
async fn workgroup_create(version: &str, name: &str, device_name: &str) -> Result<i32> {
    if let Some(client) = connect(version).await? {
        let created = client
            .workgroup_create(sapphire_bridge_api::WorkgroupCreateParams {
                name: name.to_owned(),
                device_name: device_name.to_owned(),
            })
            .await?;
        println!(
            "created workgroup {} ({}); this device is {} ({})",
            created.name, created.workgroup_id, device_name, created.device_id
        );
        return Ok(0);
    }
    let dir = BridgeDir::open()?;
    // The directory is opened first, so a format we do not understand stops us before a new
    // identity is minted.
    let node_id = this_node_id(&dir)?;
    let workgroup = Workgroup::create(&dir, name, device_name, &node_id)?;
    println!(
        "created workgroup {} ({}); this device is {} ({})",
        workgroup.name,
        workgroup.id,
        device_name,
        workgroup.this_device(&node_id)?.id
    );
    Ok(0)
}

/// Show the workgroup this host belongs to.
fn workgroup_list() -> Result<i32> {
    let dir = BridgeDir::open()?;
    match Workgroup::open(&dir)? {
        Some(workgroup) => {
            println!("{} ({})", workgroup.name, workgroup.id);
            Ok(0)
        }
        None => {
            println!("this host has not joined a workgroup");
            Ok(1)
        }
    }
}

/// List the workspaces this host serves.
///
/// Read-only by design (spec §1): the bridge knows what exists, and the app that owns a
/// workspace decides what this host keeps. The workgroup-wide list arrives with the workgroup
/// workspace itself; until then this is what the bridge's routing table holds.
async fn workspace_list(version: &str) -> Result<i32> {
    let Some(client) = connect(version).await? else {
        println!("no sapphire-bridge is running");
        return Ok(1);
    };
    let status = client.status().await?;
    if status.routes.is_empty() {
        println!("no workspaces are registered on this host");
        return Ok(0);
    }
    for route in status.routes {
        println!(
            "{} {} {}{}",
            route.workspace_id,
            route.app_name,
            route.root.display(),
            if route.owner_online { " (online)" } else { "" }
        );
    }
    Ok(0)
}

/// This host's node id, from `node.key`, creating the key if this is the first time.
#[cfg(feature = "node")]
fn this_node_id(dir: &BridgeDir) -> Result<String> {
    crate::iroh::load_or_create_node_id(&dir.node_key())
}

/// Without the `node` feature there is no key to derive a node id from.
#[cfg(not(feature = "node"))]
fn this_node_id(_dir: &BridgeDir) -> Result<String> {
    Err(Error::Config(
        "this build has no peer transport; rebuild sapphire-bridge with the `node` feature"
            .to_owned(),
    ))
}

/// A controlled environment for the CLI tests that read or set one.
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The environment variables these tests set are process-global, and `cargo test`
    /// runs a binary's tests in parallel: two tests setting them would race, and one
    /// reading another test's directory would pass by luck until it does not. One lock
    /// serializes every test that touches them — not just the ones in this file: anything
    /// anywhere in this crate that reads or sets `SAPPHIRE_RUNTIME_DIR` or
    /// [`crate::dir::BRIDGE_DIR_ENV`] directly must hold it too (see [`lock`]), or it can
    /// still race one of these (issue #165 — `dir::tests::
    /// the_environment_variable_replaces_the_whole_path` did exactly that, unguarded,
    /// reproducing a failure here 20 times out of 20 run alongside it).
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    /// Distinguishes the directories two concurrent tests may set, so a set after this
    /// test's window is never confused with this test's.
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Hold this for the duration of any test elsewhere in the crate that reads or sets
    /// `SAPPHIRE_RUNTIME_DIR` or [`crate::dir::BRIDGE_DIR_ENV`] directly, without going
    /// through [`with_dirs`]. These are the same process-global variables [`with_dirs`]
    /// serializes access to, and a test outside this module is exactly as able to race one
    /// of its tests as two of its own tests would be able to race each other.
    pub(crate) async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().await
    }

    /// Run `body` with both directories pointed inside `tmp`, and alone.
    ///
    /// Returns what `body` returned. The directories stay set until the body ends, and no
    /// other env-touching test can run while it does.
    pub(crate) async fn with_dirs<T, F>(
        tmp: &std::path::Path,
        body: impl FnOnce(std::path::PathBuf) -> F,
    ) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let _guard = ENV_LOCK.lock().await;
        let token = COUNTER.fetch_add(1, Ordering::Relaxed);
        // SAFETY: no other thread reads the environment while the lock is held.
        unsafe {
            std::env::set_var("SAPPHIRE_RUNTIME_DIR", tmp);
            std::env::set_var(
                crate::dir::BRIDGE_DIR_ENV,
                tmp.join(format!("bridge-{token}")),
            );
        }
        let value = body(tmp.join(format!("bridge-{token}"))).await;
        unsafe {
            std::env::remove_var("SAPPHIRE_RUNTIME_DIR");
            std::env::remove_var(crate::dir::BRIDGE_DIR_ENV);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(subcommand)]
        command: BridgeCommand,
    }

    #[test]
    fn the_subcommands_parse() {
        for args in [
            vec!["b", "serve"],
            vec!["b", "status"],
            vec!["b", "device", "list"],
            vec!["b", "device", "retire", "phone"],
            vec![
                "b",
                "workgroup",
                "create",
                "home",
                "--device-name",
                "laptop",
            ],
            vec!["b", "workgroup", "list"],
            vec!["b", "workspace", "list"],
            vec!["b", "service", "install"],
            vec!["b", "log", "--lines", "50"],
        ] {
            assert!(Probe::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn bare_invocation_defaults_to_serve() {
        // `sapphire-bridge` with no subcommand is `sapphire-bridge serve` (2026-09-24
        // spec decision 1, bridge edition) — main.rs expresses it with
        // `unwrap_or(BridgeCommand::Serve)`, and the default pins that.
        #[derive(Parser)]
        struct Bare {
            #[command(subcommand)]
            command: Option<BridgeCommand>,
        }
        let bare = Bare::try_parse_from(["b"]).unwrap();
        assert!(bare.command.is_none());
        assert!(matches!(BridgeCommand::default(), BridgeCommand::Serve));
    }

    #[test]
    fn the_renamed_commands_have_no_aliases() {
        // Decision 9: cut over, no aliases. `run` and `forget` are gone as spellings.
        assert!(Probe::try_parse_from(["b", "run"]).is_err());
        assert!(Probe::try_parse_from(["b", "device", "forget", "phone"]).is_err());
    }

    #[test]
    fn there_is_no_way_to_place_a_workspace_from_here() {
        // Placing a workspace on this host is the owning application's business (spec §1).
        for args in [
            vec!["b", "workspace", "map", "notes", "/tmp/x"],
            vec!["b", "workspace", "init"],
            vec!["b", "workspace", "init", "--sync"],
        ] {
            assert!(Probe::try_parse_from(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn there_is_no_stop_command() {
        // Decision 4: starting and stopping the daemon is the service manager's business.
        assert!(Probe::try_parse_from(["b", "stop"]).is_err());
    }

    #[test]
    fn the_log_subcommand_parses() {
        assert!(Probe::try_parse_from(["b", "log"]).is_ok());
        assert!(Probe::try_parse_from(["b", "log", "--follow"]).is_ok());
        assert!(Probe::try_parse_from(["b", "log", "--lines", "50"]).is_ok());
    }

    #[test]
    fn there_is_no_separate_pair_command() {
        // Pairing is `device invite` and `workgroup join`; a `pair` tree would be a second
        // spelling of the same two commands.
        assert!(Probe::try_parse_from(["b", "pair", "create"]).is_err());
    }

    #[tokio::test]
    async fn status_against_no_bridge_exits_non_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let code = test_env::with_dirs(tmp.path(), |bridge| async move {
            BridgeDir::at(bridge).unwrap();
            BridgeCommand::Status.dispatch("0.0.0").await.unwrap()
        })
        .await;
        assert_eq!(code, 1);
    }
}

#[cfg(test)]
mod status_fallback_tests {
    use super::*;
    use crate::status::PeerStatus;
    use chrono::Utc;
    use grain_id::GrainId;

    #[tokio::test]
    async fn status_without_a_bridge_reports_the_last_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let code = test_env::with_dirs(tmp.path(), |bridge| async move {
            let dir = BridgeDir::at(bridge).unwrap();
            // The bridge stopped after writing this.
            let stopped = crate::status::StatusFile {
                version: "0.14.0".into(),
                pid: std::process::id(),
                started_at: Utc::now(),
                node_id: "aaaa".into(),
                workgroup: Some(sapphire_bridge_api::WorkgroupStatus {
                    workgroup_id: GrainId::random(),
                    name: "home".into(),
                    devices: 1,
                }),
                peers: vec![PeerStatus {
                    device_id: GrainId::random(),
                    name: "phone".into(),
                    node_id: String::new(),
                    connected: false,
                    last_seen: None,
                    last_error: None,
                }],
                routes: vec![],
                embedding: Some(sapphire_bridge_api::EmbedInfoResult {
                    enabled: true,
                    model: Some(sapphire_bridge_api::EmbedModelInfo {
                        model: "all-minilm".into(),
                        dimension: 384,
                        template_version: 1,
                    }),
                    loaded: true,
                }),
                relays: vec![],
            };
            std::fs::write(
                dir.status_json(),
                serde_json::to_string_pretty(&stopped).unwrap(),
            )
            .unwrap();

            // The fallback report carries the embedding the snapshot recorded, so a
            // stopped bridge still says what it was embedding.
            let report = read_status_report().unwrap().expect("the snapshot");
            assert!(report.stale);
            let embedding = report.status.embedding.expect("an embedding line");
            assert!(embedding.enabled);
            assert_eq!(embedding.model.expect("a model").dimension, 384);

            BridgeCommand::Status.dispatch("0.0.0").await.unwrap()
        })
        .await;
        assert_eq!(code, 0, "a stopped bridge still has a story worth telling");
    }

    #[test]
    fn the_embedding_line_names_the_model_dimension_and_loaded_state() {
        use sapphire_bridge_api::{EmbedInfoResult, EmbedModelInfo};

        assert_eq!(
            embedding_line(&EmbedInfoResult::default()),
            "embedding disabled"
        );

        let enabled = EmbedInfoResult {
            enabled: true,
            model: Some(EmbedModelInfo {
                model: "all-minilm".into(),
                dimension: 384,
                template_version: 1,
            }),
            loaded: true,
        };
        assert_eq!(
            embedding_line(&enabled),
            "embedding all-minilm (384 dimension(s)), loaded"
        );

        // A model that is enabled but not yet in memory says so.
        let cold = EmbedInfoResult {
            loaded: false,
            ..enabled
        };
        assert_eq!(
            embedding_line(&cold),
            "embedding all-minilm (384 dimension(s)), not loaded"
        );
    }

    #[tokio::test]
    async fn status_without_a_bridge_or_a_snapshot_exits_non_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let code = test_env::with_dirs(tmp.path(), |bridge| async move {
            BridgeDir::at(bridge).unwrap();
            BridgeCommand::Status.dispatch("0.0.0").await.unwrap()
        })
        .await;
        assert_eq!(code, 1, "nothing to report is nothing to report");
    }
}

#[cfg(test)]
mod pairing_cli_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(subcommand)]
        command: BridgeCommand,
    }

    #[test]
    fn the_pairing_subcommands_parse() {
        for args in [
            vec!["b", "device", "invite", "--name", "phone"],
            vec!["b", "device", "invite", "--name", "phone", "--ttl", "300"],
            vec!["b", "workgroup", "join", "sapphire:ABCDEF"],
            vec![
                "b",
                "workgroup",
                "join",
                "sapphire:ABCDEF",
                "--device-name",
                "phone",
            ],
        ] {
            assert!(Probe::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn an_invite_needs_a_name() {
        assert!(Probe::try_parse_from(["b", "device", "invite"]).is_err());
    }

    #[test]
    fn a_join_needs_a_ticket() {
        assert!(Probe::try_parse_from(["b", "workgroup", "join"]).is_err());
    }

    #[tokio::test]
    async fn joining_without_a_running_bridge_says_so_rather_than_starting_one() {
        let tmp = tempfile::tempdir().unwrap();
        let result = test_env::with_dirs(tmp.path(), |bridge| async move {
            BridgeDir::at(bridge).unwrap();
            BridgeCommand::Workgroup(WorkgroupCommand::Join {
                ticket: "sapphire:ABCDEF".into(),
                device_name: Some("phone".into()),
            })
            .dispatch("0.0.0")
            .await
        })
        .await;

        match result {
            Ok(code) => assert_eq!(code, 1, "a missing bridge is a non-zero exit, not a panic"),
            Err(err) => assert!(
                !err.to_string().contains("panic"),
                "it must fail with a message, not a panic: {err}"
            ),
        }
    }

    #[test]
    fn a_ttl_is_read_as_seconds() {
        let parsed =
            Probe::try_parse_from(["b", "device", "invite", "--name", "p", "--ttl", "90"]).unwrap();
        match parsed.command {
            BridgeCommand::Device(DeviceCommand::Invite { ttl, .. }) => {
                assert_eq!(ttl, Some(90));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_bridge_service_subcommands_parse() {
        for args in [
            vec!["b", "service", "install"],
            vec!["b", "service", "uninstall"],
            vec!["b", "service", "status"],
        ] {
            assert!(Probe::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }
}

#[cfg(test)]
mod service_spec_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(subcommand)]
        command: BridgeCommand,
    }

    #[test]
    fn the_bridge_service_spec_runs_the_bridge() {
        let spec = bridge_service_spec("0.0.0");
        assert_eq!(spec.args, vec!["serve".to_owned()]);
    }

    #[test]
    fn the_bridge_service_spec_names_the_bridge() {
        let spec = bridge_service_spec("0.0.0");
        assert_eq!(spec.app_name, "sapphire-bridge");
        assert!(
            !spec.description.is_empty(),
            "a unit with an empty Description= is a unit a reader cannot identify"
        );
    }
}
