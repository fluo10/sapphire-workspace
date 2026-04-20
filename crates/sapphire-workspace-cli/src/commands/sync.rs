use std::path::Path;

use anyhow::{Result, bail};
use sapphire_workspace::{WorkspaceState, workspace::Workspace};

use crate::config::UserConfig;

use crate::WORKSPACE_CTX;

pub fn run(workspace_dir: Option<&Path>) -> Result<()> {
    let device_id = match WORKSPACE_CTX.device_id() {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::error!("could not persist device_id: {e}");
            None
        }
    };

    let (workspace, config) = open_workspace(workspace_dir)?;
    let state = WorkspaceState::open_configured(workspace, &config.sync, device_id)?;

    let Some(backend) = state.sync_backend() else {
        bail!(
            "no sync backend configured — set `backend = \"git\"` under [sync] in \
             $XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml"
        );
    };

    backend.sync()?;
    println!("sync complete");
    Ok(())
}

/// Find a workspace starting from `explicit` (or the current directory), then
/// load the user config.
pub fn open_workspace(explicit: Option<&Path>) -> Result<(Workspace, UserConfig)> {
    let start = if let Some(dir) = explicit {
        std::borrow::Cow::Owned(
            dir.canonicalize()
                .map_err(|e| anyhow::anyhow!("cannot access '{}': {e}", dir.display()))?,
        )
    } else {
        std::borrow::Cow::Owned(std::env::current_dir()?)
    };

    let workspace = Workspace::find_from(&WORKSPACE_CTX, &start)
        .or_else(|_| Workspace::resolve(&WORKSPACE_CTX, explicit))?;

    let config = crate::config::load_user_config()?;
    Ok((workspace, config))
}
