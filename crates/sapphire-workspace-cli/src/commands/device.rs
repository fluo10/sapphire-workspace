use std::path::Path;

use anyhow::{Result, bail};
use clap::Subcommand;
use sapphire_workspace::{DeviceRecord, DeviceRegistry, WorkspaceState};
use uuid::Uuid;

use crate::WORKSPACE_CTX;
use crate::commands::sync::open_workspace;

#[derive(Subcommand)]
pub enum DeviceCommand {
    /// Show all registered devices, one per line, in UUIDv7 order.
    List,
    /// Update this device's human-readable name.
    SetName {
        /// New name (stored in the `name` field).
        name: String,
    },
    /// Print full record for a device.  Argument is a UUID or a 1-based
    /// Device Number from `device list`.  Omit to show this device.
    Show {
        /// Device UUID or Device Number.
        target: Option<String>,
    },
}

pub fn run(workspace_dir: Option<&Path>, cmd: &DeviceCommand) -> Result<()> {
    match cmd {
        DeviceCommand::List => run_list(workspace_dir),
        DeviceCommand::SetName { name } => run_set_name(workspace_dir, name),
        DeviceCommand::Show { target } => run_show(workspace_dir, target.as_deref()),
    }
}

fn run_list(workspace_dir: Option<&Path>) -> Result<()> {
    let registry = open_registry(workspace_dir)?;
    let self_id = WORKSPACE_CTX.device_id();
    if registry.records().is_empty() {
        println!("(no devices registered)");
        return Ok(());
    }
    println!(
        " {:>2}  {:36}  {:20}  {:24}  {:8}  updated_at",
        "#", "id", "name", "app", "platform"
    );
    for (i, r) in registry.records().iter().enumerate() {
        let marker = if Some(r.id) == self_id { "*" } else { " " };
        let app = format!("{} {}", r.app_id, r.app_version);
        println!(
            "{}{:>2}  {:36}  {:20}  {:24}  {:8}  {}",
            marker,
            i + 1,
            r.id,
            truncate(&r.name, 20),
            truncate(&app, 24),
            truncate(&r.platform, 8),
            r.updated_at.format("%Y-%m-%d %H:%M:%SZ"),
        );
    }
    Ok(())
}

fn run_set_name(workspace_dir: Option<&Path>, name: &str) -> Result<()> {
    let (workspace, config) = open_workspace(workspace_dir)?;
    let state = WorkspaceState::open_configured(workspace, &config.sync)?;
    state.rename_device(name)?;
    let id = WORKSPACE_CTX.device_id();
    if let Some(id) = id {
        println!("device name updated to '{name}' (id: {id})");
    } else {
        println!("device name updated to '{name}'");
    }
    println!("next `sync` will commit and push this change");
    Ok(())
}

fn run_show(workspace_dir: Option<&Path>, target: Option<&str>) -> Result<()> {
    let registry = open_registry(workspace_dir)?;
    let self_id = WORKSPACE_CTX.device_id();

    let (record, number) = match target {
        None => {
            let id = self_id.ok_or_else(|| anyhow::anyhow!("cannot determine this device's id"))?;
            let r = registry
                .lookup(id)
                .ok_or_else(|| anyhow::anyhow!("this device is not registered yet"))?;
            let n = registry.device_number(id).unwrap_or(0);
            (r, n)
        }
        Some(arg) => resolve_target(&registry, arg)?,
    };

    println!("device number:  {number}");
    println!("id:             {}", record.id);
    println!("name:           {}", record.name);
    println!("hostname:       {}", record.hostname);
    println!("app:            {} {}", record.app_id, record.app_version);
    println!("platform:       {} / {}", record.platform, record.arch);
    println!("registered_at:  {}", record.registered_at.to_rfc3339());
    println!("updated_at:     {}", record.updated_at.to_rfc3339());
    if Some(record.id) == self_id {
        println!("(this device)");
    }
    Ok(())
}

fn resolve_target<'a>(
    registry: &'a DeviceRegistry,
    arg: &str,
) -> Result<(&'a DeviceRecord, usize)> {
    // Try Device Number first — UUID strings always contain hyphens, so a
    // bare integer is unambiguously a number.
    if let Ok(n) = arg.parse::<usize>() {
        if n == 0 || n > registry.records().len() {
            bail!(
                "device number {n} out of range (1..={})",
                registry.records().len()
            );
        }
        let record = &registry.records()[n - 1];
        return Ok((record, n));
    }
    let id: Uuid = arg
        .parse()
        .map_err(|e| anyhow::anyhow!("'{arg}' is neither a UUID nor a device number: {e}"))?;
    let record = registry
        .lookup(id)
        .ok_or_else(|| anyhow::anyhow!("no device with id {id} in registry"))?;
    let number = registry.device_number(id).unwrap_or(0);
    Ok((record, number))
}

/// Open the registry via `open_configured` so that the merge-on-open
/// flow (idempotent self-registration + host-field reconciliation)
/// runs before the caller reads any records — even for read-only
/// inspection commands.
fn open_registry(workspace_dir: Option<&Path>) -> Result<DeviceRegistry> {
    let (workspace, config) = open_workspace(workspace_dir)?;
    let marker_dir = workspace.marker_dir();
    let _state = WorkspaceState::open_configured(workspace, &config.sync)?;
    let path = marker_dir.join("devices.jsonl");
    Ok(DeviceRegistry::load(path)?)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
