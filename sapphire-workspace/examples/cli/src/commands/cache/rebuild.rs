use std::path::Path;

use anyhow::Result;
use sapphire_workspace::{Workspace, WorkspaceState};

pub fn run(workspace_dir: Option<&Path>) -> Result<()> {
    let workspace = Workspace::resolve(workspace_dir)?;
    let state = WorkspaceState::rebuild(workspace)?;
    let (upserted, _removed) = state.sync()?;
    println!("rebuilt: {upserted} files indexed");
    Ok(())
}
