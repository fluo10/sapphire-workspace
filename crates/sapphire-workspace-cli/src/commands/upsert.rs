use std::path::Path;

use anyhow::Result;
use sapphire_workspace::WorkspaceState;

use crate::commands::sync::{collect_device_defaults, open_workspace, resolve_device_id};

pub fn run(workspace_dir: Option<&Path>, path: &Path) -> Result<()> {
    let device_id = resolve_device_id();
    let defaults = collect_device_defaults();

    let (workspace, config) = open_workspace(workspace_dir)?;

    let state =
        WorkspaceState::open_configured(workspace, &config.sync, device_id, Some(defaults))?;

    let abs_path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };

    state.on_file_updated(&abs_path)?;
    println!("upserted: {}", abs_path.display());
    Ok(())
}
