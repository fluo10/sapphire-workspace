use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer, notify};
use sapphire_workspace::WorkspaceState;

use crate::commands::sync::open_workspace;

pub fn run(workspace_dir: Option<&Path>, debounce_ms: u64) -> Result<()> {
    // Ensure a device_id is set before loading the final config.
    if let Err(e) = crate::config::ensure_device_id() {
        eprintln!("warning: could not persist device_id: {e}");
    }

    let (workspace, config) = open_workspace(workspace_dir)?;

    let watch_root = workspace.root.clone();
    let git_sync_interval = config.sync.sync_interval();
    let retrieve_interval = config.retrieve.sync_interval();

    let state = Arc::new(WorkspaceState::open_configured(workspace, &config.sync)?);

    println!("watching: {}", watch_root.display());
    if let Some(interval) = git_sync_interval {
        println!("git sync interval: every {} min", interval.as_secs() / 60);
    }
    if let Some(interval) = retrieve_interval {
        println!(
            "retrieve refresh interval: every {} min",
            interval.as_secs() / 60
        );
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

    let mut last_git_sync = Instant::now();
    let mut last_retrieve_sync = Instant::now();

    loop {
        // Compute the next wake-up as the minimum of the two timers' remaining
        // durations (or None if neither timer is active).
        let timeout = [
            git_sync_interval.map(|i| i.saturating_sub(last_git_sync.elapsed())),
            retrieve_interval.map(|i| i.saturating_sub(last_retrieve_sync.elapsed())),
        ]
        .into_iter()
        .flatten()
        .min();

        let result = match timeout {
            Some(t) => rx.recv_timeout(t),
            None => rx
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };

        // Check and run git sync if its interval has elapsed.
        let run_git_sync = |last: &mut Instant| {
            if let Some(interval) = git_sync_interval {
                if last.elapsed() >= interval {
                    print!("git sync... ");
                    match state_clone.sync_git() {
                        Ok(()) => println!("done"),
                        Err(e) => eprintln!("git sync error: {e}"),
                    }
                    *last = Instant::now();
                }
            }
        };

        // Check and run retrieve cache refresh if its interval has elapsed.
        let run_retrieve_sync = |last: &mut Instant| {
            if let Some(interval) = retrieve_interval {
                if last.elapsed() >= interval {
                    print!("retrieve refresh... ");
                    match state_clone.sync_retrieve() {
                        Ok((upserted, removed)) => {
                            println!("done (upserted: {upserted}, removed: {removed})")
                        }
                        Err(e) => eprintln!("retrieve refresh error: {e}"),
                    }
                    *last = Instant::now();
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
                run_git_sync(&mut last_git_sync);
                run_retrieve_sync(&mut last_retrieve_sync);
            }
            Ok(Err(e)) => eprintln!("watch error: {e}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                run_git_sync(&mut last_git_sync);
                run_retrieve_sync(&mut last_retrieve_sync);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}
