use std::path::Path;

use anyhow::Result;
use sapphire_workspace::{Workspace, WorkspaceState};

use crate::WORKSPACE_CTX;
use crate::config::UserConfig;

pub fn run(workspace_dir: Option<&Path>) -> Result<()> {
    let workspace = Workspace::resolve(&WORKSPACE_CTX, workspace_dir)?;
    let config = UserConfig::load()?;
    let state = WorkspaceState::open(workspace)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (upserted, _removed, embedded) = rt.block_on(state.sync_and_embed(&config.retrieve))?;

    println!("synced: {upserted} files");
    if embedded > 0 {
        println!("embedded: {embedded} new chunks");
    }
    Ok(())
}
