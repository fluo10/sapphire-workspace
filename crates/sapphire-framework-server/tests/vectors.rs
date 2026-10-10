//! Synced vectors, end to end (#187).
//!
//! A file is embedded once, by the device that wrote it, and its vector reaches the other
//! devices as a file under `.<app>/embedded/`, which they read instead of embedding. A file
//! written by a device that does not embed is embedded by the primary device.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{NODE_A, NODE_B};
use sapphire_backend::protocol as proto;
use sapphire_bridge_api::EmbedModelInfo;
use sapphire_framework_bridge::{EmbedProvider, LoopbackNetwork};

/// A 3-dimensional model that counts the texts it embeds.
struct CountingProvider(AtomicUsize);

#[async_trait::async_trait]
impl EmbedProvider for CountingProvider {
    fn info(&self) -> Option<EmbedModelInfo> {
        Some(EmbedModelInfo {
            model: "fake-3d".into(),
            dimension: 3,
            template_version: 1,
            revision: None,
            max_tokens: None,
        })
    }
    fn loaded(&self) -> bool {
        true
    }
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.0.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|t| vec![t.len() as f32, 1.0, 0.5])
            .collect())
    }
}

fn counting() -> Arc<CountingProvider> {
    Arc::new(CountingProvider(AtomicUsize::new(0)))
}

async fn write(host: &common::Host, rel: &str, content: &str) {
    let _: proto::Ack = host
        .client
        .call(
            proto::WRITE_FILE,
            proto::ContentParams {
                ws: host.ws.clone(),
                path: PathBuf::from(rel),
                content: content.into(),
            },
        )
        .await
        .unwrap();
}

/// The vector files under `host`'s `.<app>/embedded/`, as `<profile>/<file>` names.
fn vector_files(host: &common::Host) -> Vec<String> {
    let embedded = host
        .ws
        .join(format!(".{}", common::ctx().app_name))
        .join("embedded");
    let mut out = Vec::new();
    for profile in std::fs::read_dir(&embedded).into_iter().flatten().flatten() {
        for file in std::fs::read_dir(profile.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.ends_with(".vec") {
                out.push(format!("{}/{name}", profile.file_name().to_string_lossy()));
            }
        }
    }
    out.sort();
    out
}

/// The hex SHA-256 of `content`: the vector file's name.
fn hash_of(content: &str) -> String {
    sapphire_workspace::vectors::content_hash(content.as_bytes())
}

async fn await_condition(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ok() {
        assert!(Instant::now() < deadline, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn has_vector(host: &common::Host, content: &str) -> bool {
    let want = format!("{}.vec", hash_of(content));
    vector_files(host).iter().any(|f| f.ends_with(&want))
}

fn file_at(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_author_embeds_and_the_other_device_reads_the_vector() {
    let net = LoopbackNetwork::new();
    let (pa, pb) = (counting(), counting());
    let a = common::start_host_embedding(&net, NODE_A, "a", 0, pa.clone()).await;
    let b = common::start_host_embedding(&net, NODE_B, "b", 0, pb.clone()).await;
    common::introduce_all(&[&a, &b]);
    common::enable_sync(&a).await;
    common::enable_sync(&b).await;

    let content = "a note written on A";
    write(&a, "note.md", content).await;

    await_condition("A's vector reaching B", || {
        file_at(&b.ws, "note.md").is_some() && has_vector(&b, content)
    })
    .await;
    assert!(has_vector(&a, content), "A wrote the vector it computed");
    // Let B's own pass over the arrival finish, then check nobody but A embedded.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(pa.0.load(Ordering::SeqCst), 1, "the author embedded once");
    assert_eq!(
        pb.0.load(Ordering::SeqCst),
        0,
        "the other device read the vector"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_primary_embeds_what_a_device_without_embedding_wrote() {
    let net = LoopbackNetwork::new();
    let pb = counting();
    // A does not embed; B does, and is the only candidate, so it is the primary device.
    let a = common::start_host_with_priority(&net, NODE_A, "a", 0).await;
    let b = common::start_host_embedding(&net, NODE_B, "b", 1, pb.clone()).await;
    common::introduce_all(&[&a, &b]);
    common::enable_sync(&a).await;
    common::enable_sync(&b).await;
    common::await_primary(&[&a, &b], b.device_id().await).await;

    let content = "written where nothing embeds";
    write(&a, "plain.md", content).await;

    await_condition("the primary's vector reaching A", || {
        has_vector(&a, content)
    })
    .await;
    assert_eq!(pb.0.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_primary_waits_out_the_grace_period_then_backfills_on_its_own() {
    let net = LoopbackNetwork::new();
    let pb = counting();
    let a = common::start_host_with_priority(&net, NODE_A, "a", 0).await;
    let b = common::start_host_embedding(&net, NODE_B, "b", 1, pb.clone()).await;
    let runtime_b = b.runtime().unwrap();
    runtime_b.set_backfill_timing(sapphire_framework_server::BackfillTiming {
        grace: Duration::from_secs(3600),
        interval: Duration::from_millis(300),
    });
    common::introduce_all(&[&a, &b]);
    common::enable_sync(&a).await;
    common::enable_sync(&b).await;
    common::await_primary(&[&a, &b], b.device_id().await).await;

    let content = "fresh from a device that does not embed";
    write(&a, "fresh.md", content).await;
    await_condition("the file reaching B", || {
        file_at(&b.ws, "fresh.md").is_some()
    })
    .await;
    // Several periodic passes go by inside the grace period: nothing is embedded.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        pb.0.load(Ordering::SeqCst),
        0,
        "the author's version is too fresh"
    );

    // B reports the file as pending.
    let status: proto::SyncStatusResult = b
        .client
        .call(proto::SYNC_STATUS, proto::WsParams { ws: b.ws.clone() })
        .await
        .unwrap();
    let progress = status.embedding.expect("B embeds");
    assert!(progress.pending >= 1, "{progress:?}");

    // Once the grace period is over, the next periodic pass fills it in with nothing else
    // prompting it.
    runtime_b.set_backfill_timing(sapphire_framework_server::BackfillTiming {
        grace: Duration::ZERO,
        interval: Duration::from_millis(300),
    });
    await_condition("the backfilled vector reaching A", || {
        has_vector(&a, content)
    })
    .await;
    assert_eq!(pb.0.load(Ordering::SeqCst), 1);
}
