use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer, notify};
use sapphire_workspace::WorkspaceState;

use crate::commands::sync::open_workspace;

pub fn run(workspace_dir: Option<&Path>, debounce_ms: u64) -> Result<()> {
    let (workspace, config) = open_workspace(workspace_dir)?;

    let watch_root = workspace.root.clone();

    let state = Arc::new(match config {
        Some(ref cfg) => WorkspaceState::open_configured(workspace, cfg)?,
        None => WorkspaceState::open(workspace)?,
    });

    println!("watching: {}", watch_root.display());
    println!("press Ctrl+C to stop");

    let state_clone = Arc::clone(&state);

    // Channel for debounced events.
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();

    let debounce = Duration::from_millis(debounce_ms);
    let mut debouncer: Debouncer<notify::RecommendedWatcher> =
        new_debouncer(debounce, tx)?;

    debouncer
        .watcher()
        .watch(&watch_root, notify::RecursiveMode::Recursive)?;

    for result in rx {
        let events = match result {
            Ok(events) => events,
            Err(e) => {
                eprintln!("watch error: {e}");
                continue;
            }
        };

        for event in events {
            let path = &event.path;

            // Skip paths inside hidden directories.
            if path
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
            {
                continue;
            }

            // Infer upsert vs delete from file existence (debouncer-mini doesn't
            // expose fine-grained event kinds like Create/Modify/Remove).
            if path.is_file() {
                match state_clone.on_file_updated(path) {
                    Ok(()) => println!("upserted: {}", path.display()),
                    Err(e) => eprintln!("error upserting '{}': {e}", path.display()),
                }
            } else if !path.exists() {
                match state_clone.on_file_deleted(path) {
                    Ok(()) => println!("deleted: {}", path.display()),
                    Err(e) => eprintln!("error deleting '{}': {e}", path.display()),
                }
            }
        }
    }

    Ok(())
}
