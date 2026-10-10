//! The host-wide sapphire daemon.
//!
//! Running it with no subcommand starts the bridge (`serve`); every other subcommand is a
//! one-shot command against a running one. Some work directly on the bridge directory
//! instead — `workgroup create`, because there is nothing to ask about a workgroup that does
//! not exist yet, and `device retire`, because the control plane has no method for it. Both
//! then see their effect immediately: the ledger is re-read on every authorization. The
//! `embedding` commands write the files themselves only when no bridge runs, so a device can
//! be set up before its bridge starts.
//!
//! `service install` registers this binary with the OS service manager: a unit that runs
//! `sapphire-bridge serve` and nothing else, described by [`bridge_service_spec`] through
//! the command type the bridge re-exports.

mod embed;

use clap::Parser;
use sapphire_bridge::{BridgeCommand, ServiceCommand, bridge_service_spec};

#[derive(Parser)]
#[command(name = "sapphire-bridge", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<BridgeCommand>,
}

// The `service` subcommand is this type; naming it here is what keeps it in the bridge's
// command surface and not a second dependency's.
const _: fn() = || {
    let _ = ServiceCommand::Uninstall;
    let _ = bridge_service_spec;
};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // The bridge's own subscriber: the console, and — once `serve` installs the log — the
    // file layer. `tracing` allows one global subscriber per process, so the binary does
    // not install its own and the bridge's file layer is not shut out.
    sapphire_bridge::install_console();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(BridgeCommand::Serve);
    match command
        .dispatch_with(env!("CARGO_PKG_VERSION"), Some(embed::factory()))
        .await
    {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("sapphire-bridge: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}
