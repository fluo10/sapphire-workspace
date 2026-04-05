use std::path::Path;

use anyhow::Result;
use sapphire_workspace::WorkspaceState;

use crate::commands::sync::open_workspace;

pub fn run(workspace_dir: Option<&Path>, path: &Path) -> Result<()> {
    let (workspace, config) = open_workspace(workspace_dir)?;

    let state = match config {
        Some(ref cfg) => WorkspaceState::open_configured(workspace, cfg)?,
        None => WorkspaceState::open(workspace)?,
    };

    let abs_path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };

    state.on_file_updated(&abs_path)?;
    println!("upserted: {}", abs_path.display());
    Ok(())
}
