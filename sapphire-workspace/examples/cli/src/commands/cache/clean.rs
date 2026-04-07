use std::path::Path;

use anyhow::Result;
use sapphire_workspace::Workspace;

use super::info::{find_stale_retrieve, human_size};
use crate::WORKSPACE_CTX;

pub fn run(workspace_dir: Option<&Path>) -> Result<()> {
    let workspace = Workspace::resolve(workspace_dir, &WORKSPACE_CTX)?;
    let cache_dir = workspace.cache_dir();
    let mut removed_any = false;

    // ── stale retrieve SQLite files ───────────────────────────────────────────
    for (path, size) in find_stale_retrieve(&cache_dir) {
        let base = path.to_string_lossy();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{base}{suffix}"));
        }
        println!("removed: {} ({})", path.display(), human_size(size));
        removed_any = true;
    }

    // ── stale LanceDB directories ─────────────────────────────────────────────
    #[cfg(feature = "lancedb-store")]
    for (path, size) in super::info::find_stale_lancedb(&cache_dir) {
        std::fs::remove_dir_all(&path)?;
        println!("removed: {} ({})", path.display(), human_size(size));
        removed_any = true;
    }

    if !removed_any {
        println!("nothing to clean");
    }
    Ok(())
}
