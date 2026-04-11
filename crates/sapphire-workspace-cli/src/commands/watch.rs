use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer, notify};
use sapphire_workspace::WorkspaceState;

use crate::commands::sync::open_workspace;

pub fn run(workspace_dir: Option<&Path>, debounce_ms: u64) -> Result<()> {
    let (workspace, mut config) = open_workspace(workspace_dir)?;

    // Ensure a device_id is set in the user config, then re-load the layered
    // config so the generated ID is included in the merged result.
    if let Some(ws_cfg_path) = config.as_ref().map(|_| workspace.config_path()) {
        match crate::config::ensure_device_id() {
            Ok(()) => config = Some(crate::config::load_layered(&ws_cfg_path)?),
            Err(e) => eprintln!("warning: could not persist device_id: {e}"),
        }
    }

    let watch_root = workspace.root.clone();
    let sync_interval = config.as_ref().and_then(|c| c.sync_interval());

    let state = Arc::new(match config {
        Some(ref cfg) => WorkspaceState::open_configured(workspace, cfg)?,
        None => WorkspaceState::open(workspace)?,
    });

    println!("watching: {}", watch_root.display());
    if let Some(interval) = sync_interval {
        println!("periodic sync: every {} min", interval.as_secs() / 60);
    }
    println!("press Ctrl+C to stop");

    let state_clone = Arc::clone(&state);

    // Channel for debounced events.
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();

    let debounce = Duration::from_millis(debounce_ms);
    let mut debouncer: Debouncer<notify::RecommendedWatcher> = new_debouncer(debounce, tx)?;

    debouncer
        .watcher()
        .watch(&watch_root, notify::RecursiveMode::Recursive)?;

    let mut last_sync = Instant::now();

    loop {
        // Block until an event arrives, or until the next sync is due.
        let timeout = sync_interval.map(|interval| interval.saturating_sub(last_sync.elapsed()));

        let result = match timeout {
            Some(t) => rx.recv_timeout(t),
            None => rx
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };

        // Run periodic sync when the interval has elapsed (timeout or after event processing).
        let run_periodic_sync = |last_sync: &mut Instant| {
            if let Some(interval) = sync_interval {
                if last_sync.elapsed() >= interval {
                    print!("periodic sync... ");
                    match state_clone.periodic_sync() {
                        Ok((upserted, removed)) => {
                            println!("done (upserted: {upserted}, removed: {removed})")
                        }
                        Err(e) => eprintln!("sync error: {e}"),
                    }
                    *last_sync = Instant::now();
                }
            }
        };

        match result {
            Ok(Ok(events)) => {
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
                run_periodic_sync(&mut last_sync);
            }
            Ok(Err(e)) => eprintln!("watch error: {e}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                run_periodic_sync(&mut last_sync);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}
