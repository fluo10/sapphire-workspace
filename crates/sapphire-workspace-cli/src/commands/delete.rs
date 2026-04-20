use std::path::Path;

use anyhow::Result;
use sapphire_workspace::WorkspaceState;

use crate::commands::sync::open_workspace;

pub fn run(workspace_dir: Option<&Path>, path: &Path) -> Result<()> {
    let (workspace, config) = open_workspace(workspace_dir)?;

    // Staging-only command: no commits are produced, so no device id needed.
    let state = WorkspaceState::open_configured(workspace, &config.sync, None)?;

    let abs_path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };

    state.on_file_deleted(&abs_path)?;
    println!("deleted: {}", abs_path.display());
    Ok(())
}
