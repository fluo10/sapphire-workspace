//! The egui-free half: a background task that keeps a [`Snapshot`] of the bridge and the app
//! server current, and carries out [`Command`]s one at a time.

mod conn;
mod exec;
mod types;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};

pub use conn::classify;
pub use types::*;

/// The handle the GUI holds. Dropping it stops the background task.
pub struct FrameworkClient {
    app: AppIdentity,
    snapshot: watch::Receiver<Snapshot>,
    commands: mpsc::UnboundedSender<(CommandId, Command)>,
    outcomes: Arc<Mutex<Vec<Outcome>>>,
    next_id: AtomicU64,
    task: tokio::task::JoinHandle<()>,
}

impl FrameworkClient {
    /// Start refreshing on `runtime`. `repaint` is called after every refresh and every
    /// finished command — pass `move || ctx.request_repaint()`.
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        config: ClientConfig,
        repaint: Arc<dyn Fn() + Send + Sync>,
    ) -> FrameworkClient {
        let (snap_tx, snapshot) = watch::channel(Snapshot::default());
        let (commands, cmd_rx) = mpsc::unbounded_channel();
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let app = config.app;
        let task = runtime.spawn(run(config, snap_tx, cmd_rx, Arc::clone(&outcomes), repaint));
        FrameworkClient {
            app,
            snapshot,
            commands,
            outcomes,
            next_id: AtomicU64::new(1),
            task,
        }
    }

    /// Which application this client serves.
    pub fn app(&self) -> &AppIdentity {
        &self.app
    }

    /// The latest snapshot. Borrow it for one frame; do not hold it across an await.
    pub fn snapshot(&self) -> watch::Ref<'_, Snapshot> {
        self.snapshot.borrow()
    }

    /// Queue `command`. Its [`Outcome`] arrives through [`drain_outcomes`](Self::drain_outcomes).
    pub fn send(&self, command: Command) -> CommandId {
        let id = CommandId(self.next_id.fetch_add(1, Ordering::Relaxed));
        // The task only ends when this handle drops, so the channel is open here.
        let _ = self.commands.send((id, command));
        id
    }

    /// Every outcome since the last call.
    pub fn drain_outcomes(&self) -> Vec<Outcome> {
        std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for FrameworkClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The background loop: refresh on every tick and after every command.
///
/// Commands run one at a time, in order. A join can take a few seconds; refreshes wait for
/// it, which is acceptable for a settings screen and keeps the cached connections simple.
async fn run(
    config: ClientConfig,
    snapshot: watch::Sender<Snapshot>,
    mut commands: mpsc::UnboundedReceiver<(CommandId, Command)>,
    outcomes: Arc<Mutex<Vec<Outcome>>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let mut conns = conn::Connections::default();
    let mut tick = tokio::time::interval(config.refresh);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            received = commands.recv() => {
                let Some((id, command)) = received else { return };
                let result = exec::execute(&config, &mut conns, command).await;
                outcomes.lock().unwrap_or_else(|e| e.into_inner()).push(Outcome { id, result });
            }
        }
        let bridge = conns.fetch_bridge(&config).await;
        let server = conns.fetch_server(&config).await;
        snapshot.send_replace(Snapshot {
            bridge,
            server,
            fetched: true,
        });
        repaint();
    }
}
