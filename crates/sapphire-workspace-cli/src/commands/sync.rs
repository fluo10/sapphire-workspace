use std::path::Path;

use anyhow::{Result, bail};
use sapphire_workspace::{WorkspaceConfig, WorkspaceState, workspace::Workspace};

use crate::WORKSPACE_CTX;

pub fn run(workspace_dir: Option<&Path>) -> Result<()> {
    let (workspace, config) = open_workspace(workspace_dir)?;

    let Some(config) = config else {
        bail!(
            "no .sapphire-workspace/config.toml found — run `sapphire-workspace init` first, \
             or set [sync] backend in the config"
        );
    };

    let state = WorkspaceState::open_configured(workspace, &config)?;

    let Some(backend) = state.sync_backend() else {
        bail!(
            "no sync backend configured — set `backend = \"git\"` under [sync] in \
             .sapphire-workspace/config.toml"
        );
    };

    backend.sync()?;
    println!("sync complete");
    Ok(())
}

/// Try to find a workspace with a marker directory; fall back to `resolve()`.
/// Returns the workspace and, if a marker was found, the loaded `WorkspaceConfig`.
pub fn open_workspace(
    explicit: Option<&Path>,
) -> Result<(Workspace, Option<WorkspaceConfig>)> {
    // If an explicit path was given, use it directly.
    let start = if let Some(dir) = explicit {
        std::borrow::Cow::Owned(
            dir.canonicalize()
                .map_err(|e| anyhow::anyhow!("cannot access '{}': {e}", dir.display()))?,
        )
    } else {
        std::borrow::Cow::Owned(std::env::current_dir()?)
    };

    match Workspace::find_from(&WORKSPACE_CTX, &start) {
        Ok(ws) => {
            let config = crate::config::load_layered(&ws.config_path())?;
            Ok((ws, Some(config)))
        }
        Err(_) => {
            // No marker found — fall back to legacy resolution.
            let ws = Workspace::resolve(&WORKSPACE_CTX, explicit)?;
            Ok((ws, None))
        }
    }
}
