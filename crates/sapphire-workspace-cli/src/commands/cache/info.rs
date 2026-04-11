use std::path::Path;

use anyhow::Result;
use sapphire_workspace::{
    RETRIEVE_SCHEMA_VERSION as SCHEMA_VERSION, UserConfig, VectorDb, Workspace, WorkspaceState,
};

use crate::WORKSPACE_CTX;

pub fn run(workspace_dir: Option<&Path>) -> Result<()> {
    let workspace = Workspace::resolve(&WORKSPACE_CTX, workspace_dir)?;
    let config = UserConfig::load()?;
    let state = WorkspaceState::open(workspace)?;

    let info = state.db_info()?;
    println!("workspace:      {}", state.workspace.root.display());
    println!("db path:        {}", info.db_path.display());
    println!(
        "schema version: v{} (app: v{})",
        info.schema_version, SCHEMA_VERSION
    );
    println!("documents:      {}", info.document_count);

    let stale_dbs = find_stale_retrieve(&state.workspace.cache_dir());
    if !stale_dbs.is_empty() {
        let names: Vec<String> = stale_dbs
            .iter()
            .map(|(p, _)| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let total: u64 = stale_dbs.iter().map(|(_, sz)| sz).sum();
        println!(
            "stale dbs:      {} ({}) — run `sapphire-workspace clean` to remove",
            names.join(", "),
            human_size(total)
        );
    }

    if let Some(retrieve) = &config.retrieve {
        if let Some(embed_cfg) = &retrieve.embedding {
            let enabled_str = if embed_cfg.enabled {
                "enabled"
            } else {
                "disabled"
            };
            println!(
                "embedding:      {} (provider={}, model={})",
                enabled_str, embed_cfg.provider, embed_cfg.model
            );

            match retrieve.db {
                VectorDb::None => {}
                VectorDb::SqliteVec => {
                    if embed_cfg.dimension.is_some() {
                        state
                            .load_retrieve_backend(&config)
                            .map_err(anyhow::Error::msg)?;
                        match state.retrieve_db().vec_info() {
                            Ok(vi) => {
                                println!("vector backend: sqlite_vec (dim={})", vi.embedding_dim);
                                println!(
                                    "vectors:        {} indexed, {} pending",
                                    vi.vector_count, vi.pending_count
                                );
                            }
                            Err(e) => eprintln!("warn: could not read vector stats: {e}"),
                        }
                    } else {
                        println!("vector backend: sqlite_vec (dimension not configured)");
                    }
                }
                #[cfg(feature = "lancedb-store")]
                VectorDb::LanceDb => {
                    if embed_cfg.dimension.is_some() {
                        use sapphire_workspace::lancedb_store;
                        let dir = lancedb_store::data_dir(&state.workspace.cache_dir());
                        println!("vector backend: lancedb");
                        println!("lancedb path:   {}", dir.display());
                        state
                            .load_retrieve_backend(&config)
                            .map_err(anyhow::Error::msg)?;
                        match state.retrieve_db().vec_info() {
                            Ok(vi) => {
                                println!(
                                    "vectors:        {} indexed, {} pending",
                                    vi.vector_count, vi.pending_count
                                );
                            }
                            Err(e) => eprintln!("warn: could not read vector stats: {e}"),
                        }
                        let stale = find_stale_lancedb(&state.workspace.cache_dir());
                        if !stale.is_empty() {
                            let names: Vec<String> = stale
                                .iter()
                                .map(|(p, _)| {
                                    p.file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned()
                                })
                                .collect();
                            let total: u64 = stale.iter().map(|(_, sz)| sz).sum();
                            println!(
                                "stale lancedb:  {} ({}) — run `sapphire-workspace clean` to remove",
                                names.join(", "),
                                human_size(total)
                            );
                        }
                    } else {
                        println!("vector backend: lancedb (dimension not configured)");
                    }
                }
                #[cfg(not(feature = "lancedb-store"))]
                VectorDb::LanceDb => {
                    println!("vector backend: lancedb (not compiled in)");
                }
            }
        } // end if let Some(embed_cfg)
    } // end if let Some(retrieve)

    Ok(())
}

// ── stale version discovery ───────────────────────────────────────────────────

pub fn find_stale_retrieve(cache_dir: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
    let current = format!("retrieve_v{SCHEMA_VERSION}.db");
    let Ok(rd) = std::fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let n = name.to_string_lossy();
            n.starts_with("retrieve_v") && n.ends_with(".db") && n != current.as_str()
        })
        .map(|e| {
            let p = e.path();
            let sz = p.metadata().map(|m| m.len()).unwrap_or(0);
            (p, sz)
        })
        .collect()
}

#[cfg(feature = "lancedb-store")]
pub fn find_stale_lancedb(cache_dir: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
    let current = format!(
        "lancedb_full_v{}",
        sapphire_workspace::lancedb_store::SCHEMA_VERSION
    );
    let Ok(rd) = std::fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let n = name.to_string_lossy();
            let suffix = n.strip_prefix("lancedb_full_v").unwrap_or("");
            !suffix.is_empty() && suffix.parse::<i32>().is_ok() && n != current.as_str()
        })
        .map(|e| {
            let p = e.path();
            let sz = dir_size(&p);
            (p, sz)
        })
        .collect()
}

fn dir_size(path: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                dir_size(&p)
            } else {
                p.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

pub fn human_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes} B")
    }
}
