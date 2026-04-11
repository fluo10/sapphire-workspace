//! Layered config loading for `sapphire-workspace-cli`.
//!
//! Uses the [`config`](https://docs.rs/config) crate to stack the
//! workspace-level file (`{marker}/config.toml`, shared defaults synced
//! across devices) with the per-user override file
//! (`$XDG_CONFIG_HOME/sapphire-workspace/config.toml`, host-specific
//! settings such as the embedding model).
//!
//! The per-user file wins key-by-key: fields it doesn't set inherit
//! from the workspace-level file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use config::{Config, File, FileFormat};
use sapphire_workspace::WorkspaceConfig;

use crate::WORKSPACE_CTX;

/// Per-user config path: `$XDG_CONFIG_HOME/{app_name}/config.toml`.
pub fn user_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(WORKSPACE_CTX.app_name)
        .join("config.toml")
}

/// Load the workspace-level config and overlay the per-user config on top.
///
/// Sources (later wins, merged key-by-key by the `config` crate):
/// 1. `workspace_config_path` — shared defaults
/// 2. [`user_config_path`] — host-specific overrides
///
/// Both files are optional; if neither exists, the default
/// `WorkspaceConfig` is returned.
pub fn load_layered(workspace_config_path: &Path) -> Result<WorkspaceConfig> {
    let user_path = user_config_path();

    let builder = Config::builder()
        .add_source(
            File::from(workspace_config_path.to_owned())
                .format(FileFormat::Toml)
                .required(false),
        )
        .add_source(
            File::from(user_path.clone())
                .format(FileFormat::Toml)
                .required(false),
        );

    let settings = builder.build().with_context(|| {
        format!(
            "failed to load layered config from '{}' + '{}'",
            workspace_config_path.display(),
            user_path.display()
        )
    })?;

    settings
        .try_deserialize::<WorkspaceConfig>()
        .with_context(|| {
            format!(
                "failed to deserialize layered config ('{}' + '{}')",
                workspace_config_path.display(),
                user_path.display()
            )
        })
}
