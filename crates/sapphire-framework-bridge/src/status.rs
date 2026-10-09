//! `status.json`: what the bridge is doing, for anyone on this host to read.
//!
//! The file is rewritten every [`STATUS_INTERVAL`] and promptly when its content changes,
//! atomically: through a temporary file and a rename. A reader that caught a half-written
//! `status.json` would report nonsense at exactly the moment someone is trying to find out
//! what is wrong. When the bridge stops, the last snapshot stays in place — which is how
//! `sapphire-bridge status` tells you what the bridge was doing before it stopped.
//!
//! See `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §5.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use grain_id::GrainId;
use sapphire_bridge_api::{EmbedInfoResult, RouteStatus, WorkgroupStatus};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::net::NetConfig;

/// How often `status.json` is rewritten even when nothing changed.
pub const STATUS_INTERVAL: Duration = Duration::from_secs(5);

/// How often the source is polled for a change between rewrites.
///
/// A change is therefore visible in the file within this long, without rewriting the file
/// on every poll when nothing did change.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What the bridge knows, in the form `status.json` carries it.
///
/// A superset of the control plane's [`sapphire_bridge_api::StatusResult`]: a reader of the
/// file cannot call `bridge.peers`, so the devices, the relays and when this bridge started
/// travel in the file instead.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusFile {
    /// The bridge's version.
    pub version: String,
    /// The bridge process's id.
    pub pid: u32,
    /// When this bridge started.
    pub started_at: DateTime<Utc>,
    /// This host's iroh node id.
    pub node_id: String,
    /// The workgroup this host belongs to, if any.
    pub workgroup: Option<WorkgroupStatus>,
    /// Every non-retired device of the workgroup.
    pub peers: Vec<PeerStatus>,
    /// Every workspace an app server of this host owns.
    pub routes: Vec<RouteStatus>,
    /// The embedding service, when this bridge reports one — the same line the control
    /// plane's `embed.info` answers with, so a stopped bridge's snapshot still says what it
    /// was embedding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbedInfoResult>,
    /// The relay URLs this host's endpoint uses.
    pub relays: Vec<String>,
}

impl StatusFile {
    /// Read the last snapshot, if the bridge has written one yet.
    ///
    /// A missing file is `None` rather than an error: the bridge has never run here, which
    /// is a normal state for `sapphire-bridge status` to report. A file that is there but
    /// unreadable is an error — guessing at what a broken snapshot meant is worse than
    /// reporting it.
    pub fn load(path: &Path) -> Result<Option<StatusFile>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(e)),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))
    }
}

/// One device of the workgroup, as the status file reports it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeerStatus {
    /// Its device id.
    pub device_id: GrainId,
    /// Its name.
    pub name: String,
    /// Its iroh node id; empty when the device has never announced one.
    pub node_id: String,
    /// Whether the bridge currently holds a connection to it.
    pub connected: bool,
    /// When the bridge last heard from it.
    ///
    /// The bridge does not track that yet; the field is there for when it does.
    pub last_seen: Option<DateTime<Utc>>,
    /// What the last exchange with it failed with.
    ///
    /// The bridge does not track that yet; the field is there for when it does.
    pub last_error: Option<String>,
}

/// Where the data in `status.json` comes from.
///
/// The bridge implements this against its live tables; tests answer from a fixed snapshot.
/// A snapshot must be cheap: the writer polls it every [`POLL_INTERVAL`].
pub trait StatusSource: Send + Sync + 'static {
    /// What the bridge knows right now.
    fn snapshot(&self) -> StatusFile;
}

/// The running bridge, as a [`StatusSource`].
///
/// Built when the bridge's loops start. It holds the resolved network configuration, which
/// the bridge itself no longer keeps once it has handed it to the loops, and the moment the
/// bridge started.
pub(crate) struct BridgeSource {
    bridge: std::sync::Arc<crate::Bridge>,
    net: NetConfig,
    started_at: DateTime<Utc>,
}

impl BridgeSource {
    /// A source reporting `bridge`'s state, served with `net`.
    pub(crate) fn new(
        bridge: std::sync::Arc<crate::Bridge>,
        net: NetConfig,
        started_at: DateTime<Utc>,
    ) -> BridgeSource {
        BridgeSource {
            bridge,
            net,
            started_at,
        }
    }
}

impl StatusSource for BridgeSource {
    fn snapshot(&self) -> StatusFile {
        // Every block degrades on its own: a ledger that cannot be read right now costs the
        // file its device list, not the whole file. The status file exists to answer "what
        // is the bridge doing", and half an answer still answers more than no file.
        let workgroup = match self.bridge.workgroup() {
            Ok(workgroup) => workgroup,
            Err(err) => {
                tracing::warn!("status.json will not name a workgroup: {err}");
                None
            }
        };
        let workgroup_status = match &workgroup {
            Some(workgroup) => match crate::control::workgroup_status(workgroup) {
                Ok(status) => Some(status),
                Err(err) => {
                    tracing::warn!("status.json will not name a workgroup: {err}");
                    None
                }
            },
            None => None,
        };
        let peers = match &workgroup {
            Some(workgroup) => match crate::control::peer_infos(&self.bridge, workgroup) {
                Ok(peers) => peers
                    .into_iter()
                    .map(|peer| PeerStatus {
                        device_id: peer.device_id,
                        name: peer.name,
                        node_id: peer.node_id,
                        connected: peer.connected,
                        // The bridge does not track when it last heard from a device, or
                        // what the last exchange failed with; the fields are there for when
                        // it does.
                        last_seen: None,
                        last_error: None,
                    })
                    .collect(),
                Err(err) => {
                    tracing::warn!("status.json will list no devices: {err}");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let relays = match crate::relay::relays(&self.net, workgroup.as_ref()) {
            Ok(config) => config.urls,
            Err(err) => {
                tracing::warn!("status.json will name no relays: {err}");
                Vec::new()
            }
        };

        StatusFile {
            version: self.bridge.version().to_owned(),
            pid: std::process::id(),
            started_at: self.started_at,
            node_id: self.bridge.transport().node_id(),
            workgroup: workgroup_status,
            peers,
            routes: crate::control::route_statuses(&self.bridge),
            // The same source the control plane's `embed.info` handler reads, so a stopped
            // bridge's snapshot records what it was embedding when it stopped.
            embedding: Some(crate::control::embed_info(&self.bridge)),
            relays,
        }
    }
}

/// Rewrites `status.json` as the bridge's state changes.
///
/// [`StatusWriter::start`] writes the first snapshot at once and keeps the file current
/// afterwards: it re-snapshots the source every [`POLL_INTERVAL`], writing when the content
/// changed or [`STATUS_INTERVAL`] has passed since the last write. Every write is atomic —
/// through a temporary file and a rename — so a reader never catches a half-written file.
///
/// Stopping the writer leaves the last snapshot in place, which is how
/// `sapphire-bridge status` reports what a stopped bridge was doing.
pub struct StatusWriter {
    /// Dropping it closes the channel, which ends the writer task.
    _stop: tokio::sync::mpsc::Sender<()>,
}

impl StatusWriter {
    /// Start writing `path` from `source` on the current tokio runtime, and return the
    /// handle that stops it.
    ///
    /// The writer runs until the handle is dropped, so a bridge keeps its status current for
    /// exactly as long as it keeps the writer.
    pub fn start(path: PathBuf, source: Arc<dyn StatusSource>) -> StatusWriter {
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let mut last_body: Option<String> = None;
            let mut last_write: Option<Instant> = None;
            loop {
                match serde_json::to_string_pretty(&source.snapshot()) {
                    Ok(body) => {
                        // The first write is due at once, so the file appears as soon as the
                        // bridge is up rather than at the first tick.
                        let due = last_write.is_none_or(|at| at.elapsed() >= STATUS_INTERVAL);
                        if last_body.as_deref() != Some(body.as_str()) || due {
                            // The empty header keeps `write_atomic` from prefixing anything
                            // to the JSON; JSON has no comment form to spend a header on.
                            match crate::routes::write_atomic(&path, "", &body) {
                                Ok(()) => {
                                    last_write = Some(Instant::now());
                                    last_body = Some(body);
                                }
                                // A failed write is retried at the next poll: the file is
                                // stale for a moment, which a reader survives.
                                Err(err) => {
                                    tracing::warn!("could not write status.json: {err}");
                                }
                            }
                        }
                    }
                    Err(err) => tracing::warn!("could not encode status.json: {err}"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                    // Fires on a stop *and* when the handle is dropped; either way, done.
                    _ = stop_rx.recv() => break,
                }
            }
        });
        StatusWriter { _stop: stop_tx }
    }

    /// Stop rewriting, leaving the last snapshot in place.
    ///
    /// The task ends between writes, so a stop never interrupts one: no half-written
    /// temporary file is left behind, and a reader keeps seeing the last complete snapshot.
    pub fn stop(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(StatusFile);

    impl StatusSource for Fixed {
        fn snapshot(&self) -> StatusFile {
            self.0.clone()
        }
    }

    fn sample() -> StatusFile {
        StatusFile {
            version: "0.0.0".into(),
            pid: std::process::id(),
            started_at: chrono::Utc::now(),
            node_id: "abc".into(),
            workgroup: None,
            peers: vec![],
            routes: vec![],
            embedding: None,
            relays: vec![],
        }
    }

    /// An embedding provider that reports a 2-dimensional "fake" model.
    struct FakeProvider;

    #[async_trait::async_trait]
    impl crate::EmbedProvider for FakeProvider {
        fn info(&self) -> Option<sapphire_bridge_api::EmbedModelInfo> {
            Some(sapphire_bridge_api::EmbedModelInfo {
                model: "fake".into(),
                dimension: 2,
                template_version: 1,
            })
        }
        fn loaded(&self) -> bool {
            true
        }
        async fn embed(&self, texts: Vec<String>) -> std::result::Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
        }
    }

    /// The status file is filled from the bridge's own provider — the same source
    /// `embed.info` answers from — so a stopped bridge's snapshot still records what it was
    /// embedding.
    #[test]
    fn the_snapshot_records_what_the_bridge_is_embedding() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::BridgeDir::at(tmp.path().join("bridge")).unwrap();
        crate::workgroup::Workgroup::create(&dir, "test", "host-a", "aaaa").unwrap();
        let bridge = Arc::new(
            crate::Bridge::new(
                dir,
                Arc::new(crate::LoopbackNetwork::new().transport("aaaa")),
                "0.0.0",
            )
            .unwrap()
            .net(NetConfig::default())
            .embed_provider(Arc::new(FakeProvider)),
        );

        let source = BridgeSource::new(bridge, NetConfig::default(), chrono::Utc::now());
        let snapshot = source.snapshot();

        let embedding = snapshot.embedding.expect("an embedding line");
        assert!(embedding.enabled);
        assert!(embedding.loaded);
        assert_eq!(embedding.model.expect("a model").model, "fake");
    }

    /// A bridge with no provider still carries the line, disabled — the file always answers
    /// "does this host embed".
    #[test]
    fn the_snapshot_reports_embedding_disabled_without_a_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::BridgeDir::at(tmp.path().join("bridge")).unwrap();
        crate::workgroup::Workgroup::create(&dir, "test", "host-a", "aaaa").unwrap();
        let bridge = Arc::new(
            crate::Bridge::new(
                dir,
                Arc::new(crate::LoopbackNetwork::new().transport("aaaa")),
                "0.0.0",
            )
            .unwrap()
            .net(NetConfig::default()),
        );

        let source = BridgeSource::new(bridge, NetConfig::default(), chrono::Utc::now());
        let snapshot = source.snapshot();

        let embedding = snapshot.embedding.expect("an embedding line");
        assert!(!embedding.enabled);
        assert!(embedding.model.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_file_appears_promptly_and_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        let writer = StatusWriter::start(path.clone(), Arc::new(Fixed(sample())));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "no status.json was written"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: StatusFile = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.node_id, "abc");
        writer.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reader_never_sees_a_half_written_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        let mut big = sample();
        big.peers = (0..2000)
            .map(|n| PeerStatus {
                device_id: grain_id::GrainId::random(),
                name: format!("device-{n}"),
                node_id: "x".repeat(64),
                connected: true,
                last_seen: Some(chrono::Utc::now()),
                last_error: None,
            })
            .collect();
        let writer = StatusWriter::start(path.clone(), Arc::new(Fixed(big)));

        // Read repeatedly while it is being rewritten; every read must parse.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut reads = 0;
        while std::time::Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&path) {
                serde_json::from_str::<StatusFile>(&text)
                    .expect("a partially written status.json reached a reader");
                reads += 1;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(reads > 10, "the test did not actually read anything");
        writer.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_the_writer_leaves_the_last_snapshot_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        let writer = StatusWriter::start(path.clone(), Arc::new(Fixed(sample())));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        writer.stop();
        assert!(
            path.exists(),
            "the last status is useful after a clean stop"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_temporary_files_are_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        let writer = StatusWriter::start(path.clone(), Arc::new(Fixed(sample())));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        writer.stop();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let leftovers: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "status.json")
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// A source whose answer changes once a flag is set.
    struct Flipping(Arc<std::sync::atomic::AtomicBool>);

    impl StatusSource for Flipping {
        fn snapshot(&self) -> StatusFile {
            let mut status = sample();
            if self.0.load(std::sync::atomic::Ordering::Relaxed) {
                status.node_id = "def".into();
            }
            status
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_is_written_without_waiting_for_the_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = StatusWriter::start(path.clone(), Arc::new(Flipping(Arc::clone(&flag))));

        let wait_for = |node: &'static str| {
            let path = path.clone();
            async move {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    if let Ok(text) = std::fs::read_to_string(&path)
                        && let Ok(parsed) = serde_json::from_str::<StatusFile>(&text)
                        && parsed.node_id == node
                    {
                        return;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "status.json never reported node {node}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        };

        wait_for("abc").await;
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        wait_for("def").await;
        writer.stop();
    }

    #[test]
    fn a_missing_file_is_no_snapshot_and_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            StatusFile::load(&tmp.path().join("status.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_unreadable_snapshot_is_an_error_not_a_guess() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(StatusFile::load(&path).is_err());
    }
}
