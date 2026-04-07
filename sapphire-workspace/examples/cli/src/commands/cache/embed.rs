use std::{io::Write as _, path::Path};

use anyhow::Result;
use sapphire_workspace::{UserConfig, VectorDb, Workspace, WorkspaceState};

use crate::WORKSPACE_CTX;

pub fn run(workspace_dir: Option<&Path>) -> Result<()> {
    let workspace = Workspace::resolve(&WORKSPACE_CTX, workspace_dir)?;
    let config = UserConfig::load()?;

    let embed_cfg = config.embedding.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "[embedding] section is required in {}",
            UserConfig::path().display()
        )
    })?;
    if !embed_cfg.enabled {
        anyhow::bail!(
            "embedding.enabled is false in {}",
            UserConfig::path().display()
        );
    }
    if embed_cfg.vector_db == VectorDb::None {
        anyhow::bail!(
            "embedding.vector_db is \"none\" — set it to \"sqlite_vec\" or \"lancedb\""
        );
    }
    if embed_cfg.dimension.is_none() {
        anyhow::bail!(
            "`dimension` is required in [embedding] (e.g. dimension = 1536 for text-embedding-3-small)"
        );
    }

    let state = WorkspaceState::open(workspace)?;
    state.sync()?;

    let progress = |done: usize, total: usize| {
        eprint!("\rembedding chunks: {done}/{total}");
        let _ = std::io::stderr().flush();
    };

    let total_embedded = state
        .embed_pending(&config, progress)
        .map_err(anyhow::Error::msg)?;

    if total_embedded > 0 {
        eprintln!(); // newline after progress line
        println!("embedded: {total_embedded} chunks");
    } else {
        println!("all chunks already have embeddings");
    }
    Ok(())
}
