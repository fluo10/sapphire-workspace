//! An [`Embedder`] that asks the bridge.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use sapphire_bridge_api::{BridgeClient, EmbedInfoResult, EmbedModelInfo, EmbedResult};
use sapphire_ipc::Endpoint;
use sapphire_retrieve::{Embedder, Error, Result};

/// The `kind` this embedder introduces itself as, for the bridge's client table.
const CLIENT_KIND: &str = "workspace";

/// How long to wait for the bridge to answer `embed.info` before treating it as "no
/// embedder".
///
/// A local IPC call answers in milliseconds; a bridge that accepts the connection and then
/// stalls must not hang [`BridgeEmbedder::connect`] — and through it `load_embedder` —
/// for ever. Two seconds is far more than a healthy bridge needs and short enough that a
/// stalled one is a hiccup rather than a hang.
const EMBED_INFO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// One unit of work for the embedder's thread.
enum Job {
    /// Embed `texts` and reply with the bridge's answer, or the reason there is none.
    Embed {
        /// The texts to embed, in the caller's order.
        texts: Vec<String>,
        /// Where the answer goes.
        reply: Sender<Result<EmbedResult>>,
    },
    /// Ask `embed.info` again and reply with the model now served (`None`: embedding is
    /// off), or an error when the bridge cannot be reached.
    Info {
        /// Where the answer goes.
        reply: Sender<Result<Option<EmbedModelInfo>>>,
    },
}

/// An [`Embedder`] that asks the bridge's `embed.embed` over the control plane.
///
/// The connection lives on a thread of its own, with its own current-thread runtime, so
/// [`embed_texts`](Embedder::embed_texts) may be called from **any** thread — a runtime
/// worker included — without one runtime blocking on another. The thread exits when the
/// `BridgeEmbedder` is dropped.
pub struct BridgeEmbedder {
    /// The jobs channel. A `Mutex` because a `std` `Sender` is `Send` but not `Sync`,
    /// and an [`Embedder`] must be both.
    tx: Mutex<Sender<Job>>,
    /// The model this embedder's vectors are for: what the bridge answered when it was last
    /// asked. An `embed.embed` answer from another model is an error.
    info: Mutex<EmbedModelInfo>,
}

impl BridgeEmbedder {
    /// Ask the bridge at `endpoint` (default: the standard bridge endpoint) whether embedding is
    /// enabled. Returns `None` when the bridge is absent or embedding is disabled.
    pub fn connect(endpoint: Option<Endpoint>) -> Option<(BridgeEmbedder, EmbedModelInfo)> {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<Job>();
        let (info_tx, info_rx) = std::sync::mpsc::channel::<Option<EmbedModelInfo>>();

        thread::Builder::new()
            .name("bridge-embedder".to_owned())
            .spawn(move || serve(jobs_rx, &info_tx, endpoint))
            .expect("spawning the embedder thread");

        // The thread reports the model, `None` (no bridge / embedding off), or nothing at
        // all if it panicked. Every one but the first means "no embedder". The wait is
        // bounded: a bridge that connects and then stalls must not hang this caller, which
        // is on the path of `load_embedder`. The worker's own `embed_info` timeout usually
        // fires first; this is the belt-and-braces guard for a worker stuck before it can
        // even send.
        match info_rx.recv_timeout(EMBED_INFO_TIMEOUT) {
            Ok(Some(info)) => {
                let embedder = BridgeEmbedder {
                    tx: Mutex::new(jobs_tx),
                    info: Mutex::new(info.clone()),
                };
                Some((embedder, info))
            }
            Ok(None) | Err(_) => None,
        }
    }

    /// The model this embedder's vectors are for.
    pub fn info(&self) -> EmbedModelInfo {
        self.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Accept vectors of `info` from now on: the caller has reconfigured its store for it.
    pub(crate) fn set_info(&self, info: EmbedModelInfo) {
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = info;
    }

    /// Ask the bridge, on this embedder's connection, which model it serves now. `Ok(None)`
    /// when it no longer embeds; an error when it cannot be reached. Blocks the caller.
    pub fn current(&self) -> Result<Option<EmbedModelInfo>> {
        let (reply, rx) = std::sync::mpsc::channel();
        self.send(Job::Info { reply })?;
        rx.recv().map_err(|_| gone())?
    }

    /// Hand a job to the thread. The lock is held only that long: the thread answers
    /// through the job's own channel, so callers never contend on the reply.
    fn send(&self, job: Job) -> Result<()> {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        tx.send(job).map_err(|_| gone())
    }
}

fn gone() -> Error {
    Error::Embed("the bridge embedder thread is gone".to_owned())
}

/// The embedder thread: connect once, report the model, then answer jobs until the
/// [`BridgeEmbedder`] drops (which drops the `Sender`, and `recv` then fails).
fn serve(
    jobs: Receiver<Job>,
    info_tx: &Sender<Option<EmbedModelInfo>>,
    endpoint: Option<Endpoint>,
) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        let _ = info_tx.send(None);
        return;
    };

    let Some(mut client) = rt.block_on(connect_client(endpoint.as_ref())) else {
        let _ = info_tx.send(None);
        return;
    };
    match probe(&rt, &client) {
        Ok(info) => match served(info) {
            Some(model) => {
                let _ = info_tx.send(Some(model));
            }
            None => {
                let _ = info_tx.send(None);
                return;
            }
        },
        Err(err) => {
            tracing::debug!("{err}");
            let _ = info_tx.send(None);
            return;
        }
    }

    while let Ok(job) = jobs.recv() {
        match job {
            Job::Embed { texts, reply } => {
                let mut result = rt.block_on(client.embed(texts.clone()));
                if result.as_ref().is_err_and(is_connection_error) {
                    // The bridge restarted, or this connection went stale: reconnect once
                    // and retry the same batch. A second failure is reported to the caller.
                    if let Some(fresh) = rt.block_on(connect_client(endpoint.as_ref())) {
                        client = fresh;
                        result = rt.block_on(client.embed(texts));
                    }
                }
                let _ = reply.send(result.map_err(|e| Error::Embed(e.to_string())));
            }
            Job::Info { reply } => {
                let mut result = probe(&rt, &client);
                if result.is_err()
                    && let Some(fresh) = rt.block_on(connect_client(endpoint.as_ref()))
                {
                    client = fresh;
                    result = probe(&rt, &client);
                }
                let _ = reply.send(result.map(served).map_err(Error::Embed));
            }
        }
    }
}

/// Ask `embed.info`, bounded: a bridge that accepted the connection and then stalls must be
/// treated as "no answer", not waited on for ever.
fn probe(
    rt: &tokio::runtime::Runtime,
    client: &BridgeClient,
) -> std::result::Result<EmbedInfoResult, String> {
    // The whole probe runs inside the runtime: `embed_info` builds a future that needs the
    // reactor, so it must be constructed inside `block_on`, not passed to it.
    match rt.block_on(async { tokio::time::timeout(EMBED_INFO_TIMEOUT, client.embed_info()).await })
    {
        Ok(Ok(info)) => Ok(info),
        Ok(Err(err)) => Err(format!("the bridge did not answer embed.info: {err}")),
        Err(_elapsed) => Err(format!(
            "the bridge did not answer embed.info within {EMBED_INFO_TIMEOUT:?}"
        )),
    }
}

/// The model an `embed.info` answer says is served, if embedding is on.
fn served(info: EmbedInfoResult) -> Option<EmbedModelInfo> {
    info.model.filter(|_| info.enabled)
}

/// Whether the bridge connection itself is broken, rather than the call having failed.
///
/// Only these are worth a reconnect: an RPC error is the bridge answering, and its answer
/// ("the provider is down") would not change on a fresh socket.
fn is_connection_error(err: &sapphire_ipc::Error) -> bool {
    use sapphire_ipc::Error as Ipc;
    matches!(
        err,
        Ipc::Io(_) | Ipc::Closed | Ipc::Protocol(_) | Ipc::Timeout(_) | Ipc::NotRunning(_)
    )
}

/// Connect to the bridge at `endpoint`, or to the standard one when it is `None`.
///
/// `None` also covers an absent bridge and a failed connection: both mean "this host
/// cannot embed", not an error the caller must handle.
async fn connect_client(endpoint: Option<&Endpoint>) -> Option<BridgeClient> {
    let version = env!("CARGO_PKG_VERSION");
    match endpoint {
        Some(ep) => BridgeClient::connect_at(ep, CLIENT_KIND, version)
            .await
            .ok()
            .flatten(),
        None => BridgeClient::connect(CLIENT_KIND, version).await.ok(),
    }
}

impl Embedder for BridgeEmbedder {
    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(Job::Embed {
            texts: texts.iter().map(|t| (*t).to_owned()).collect(),
            reply: reply_tx,
        })?;
        let answer = reply_rx.recv().map_err(|_| gone())??;
        // The bridge switched models since this embedder was configured: these vectors
        // cannot be compared with the stored ones. The next sync reconfigures the store.
        let info = self.info();
        if answer.model != info.model || answer.dimension != info.dimension {
            return Err(Error::Embed(format!(
                "the bridge now embeds with {} ({} dimensions), not {} ({} dimensions)",
                answer.model, answer.dimension, info.model, info.dimension
            )));
        }
        Ok(answer.vectors)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A fake bridge control plane that answers `embed.info` and `embed.embed`.

    use std::sync::Arc;

    use sapphire_bridge_api::{
        EMBED, EMBED_INFO, EmbedInfoResult, EmbedModelInfo, EmbedParams, EmbedResult,
    };
    use sapphire_ipc::{Endpoint, ManagedBy, Router, RpcError, ServerInfo, serve};

    /// A fake bridge listening on `endpoint`, on a runtime of its own.
    ///
    /// Its vectors are `[index, text length]`, so a test can see the order was kept.
    pub(crate) struct FakeBridge {
        pub(crate) endpoint: Endpoint,
        /// What `embed.info` answers, and which model `embed.embed` uses; changeable.
        info: Arc<std::sync::Mutex<EmbedInfoResult>>,
        rt: Option<tokio::runtime::Runtime>,
    }

    impl FakeBridge {
        /// Serve `embed.info` with `info` at `<dir>/bridge`.
        pub(crate) fn start(dir: &std::path::Path, info: EmbedInfoResult) -> FakeBridge {
            let endpoint = Endpoint::in_dir("bridge", dir.to_path_buf());
            let info = Arc::new(std::sync::Mutex::new(info));
            let (for_info, for_embed) = (Arc::clone(&info), Arc::clone(&info));
            let router = Arc::new(
                Router::new()
                    .method(EMBED_INFO, move |_| {
                        let info = for_info.lock().unwrap().clone();
                        async move {
                            serde_json::to_value(info)
                                .map_err(|e| RpcError::internal(e.to_string()))
                        }
                    })
                    .method(EMBED, move |ctx| {
                        let model = for_embed.lock().unwrap().model.clone();
                        async move {
                            let params: EmbedParams = serde_json::from_value(ctx.params)
                                .map_err(|e| RpcError::invalid_params(e.to_string()))?;
                            let Some(model) = model else {
                                return Err(RpcError::internal("embedding is disabled".to_owned()));
                            };
                            let vectors = params
                                .texts
                                .iter()
                                .enumerate()
                                .map(|(i, t)| {
                                    let mut v = vec![0.0; model.dimension as usize];
                                    v[0] = i as f32;
                                    v[1] = t.len() as f32;
                                    v
                                })
                                .collect();
                            serde_json::to_value(EmbedResult {
                                model: model.model,
                                dimension: model.dimension,
                                vectors,
                            })
                            .map_err(|e| RpcError::internal(e.to_string()))
                        }
                    }),
            );

            // Bind and start the accept loop on a thread of its own: a test may call this
            // from inside a tokio runtime (see `embed_texts_works_from_inside_a_tokio_runtime`),
            // and `block_on` on the calling thread would panic there. The thread hands the
            // runtime back once the endpoint is bound, so `start` returns only after it can
            // be connected to.
            let (rt_tx, rt_rx) = std::sync::mpsc::channel();
            let ep = endpoint.clone();
            std::thread::Builder::new()
                .name("fake-bridge".to_owned())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(1)
                        .enable_all()
                        .build()
                        .expect("fake bridge runtime");
                    rt.block_on(async move {
                        #[cfg(unix)]
                        let listener = sapphire_ipc::bind(&ep).await.expect("bind the fake bridge");
                        #[cfg(windows)]
                        let mut listener = sapphire_ipc::bind(&ep).expect("bind the fake bridge");
                        tokio::spawn(async move {
                            while let Ok(conn) = listener.accept().await {
                                let router = Arc::clone(&router);
                                tokio::spawn(async move {
                                    let info = ServerInfo {
                                        version: "fake".into(),
                                        api: sapphire_bridge_api::API_VERSION,
                                        pid: std::process::id(),
                                        managed_by: ManagedBy::Service,
                                    };
                                    let _ = serve(conn, router, "bridge", info).await;
                                });
                            }
                        });
                    });
                    let _ = rt_tx.send(rt);
                })
                .expect("spawn the fake bridge thread");
            let rt = rt_rx.recv().expect("the fake bridge bound its endpoint");

            FakeBridge {
                endpoint,
                info,
                rt: Some(rt),
            }
        }

        /// Answer with `info` from now on, as a bridge whose settings changed.
        pub(crate) fn set_info(&self, info: EmbedInfoResult) {
            *self.info.lock().unwrap() = info;
        }

        /// A bridge that embeds with a 2-dimensional model.
        pub(crate) fn enabled(dir: &std::path::Path) -> FakeBridge {
            FakeBridge::start(
                dir,
                EmbedInfoResult {
                    enabled: true,
                    model: Some(EmbedModelInfo {
                        model: "fake-2d".into(),
                        dimension: 2,
                        template_version: 1,
                        revision: None,
                        max_tokens: None,
                    }),
                    loaded: true,
                    note: None,
                },
            )
        }
    }

    impl FakeBridge {
        /// Stop listening and close every connection, waiting until that is done.
        pub(crate) fn stop(mut self) {
            if let Some(rt) = self.rt.take() {
                rt.shutdown_timeout(std::time::Duration::from_secs(5));
            }
        }
    }

    /// A fake bridge that accepts a connection and then answers nothing.
    ///
    /// Every accepted connection is held open, never read or written: a client that
    /// connects here waits on the handshake for ever, which is exactly the stall a bounded
    /// [`BridgeEmbedder::connect`] must survive.
    pub(crate) struct StallingBridge {
        pub(crate) endpoint: Endpoint,
        rt: Option<tokio::runtime::Runtime>,
    }

    impl StallingBridge {
        /// Bind at `<dir>/bridge` and accept connections without answering them.
        pub(crate) fn start(dir: &std::path::Path) -> StallingBridge {
            let endpoint = Endpoint::in_dir("bridge", dir.to_path_buf());
            let (rt_tx, rt_rx) = std::sync::mpsc::channel();
            let ep = endpoint.clone();
            std::thread::Builder::new()
                .name("stalling-bridge".to_owned())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(1)
                        .enable_all()
                        .build()
                        .expect("stalling bridge runtime");
                    rt.block_on(async move {
                        #[cfg(unix)]
                        let listener = sapphire_ipc::bind(&ep)
                            .await
                            .expect("bind the stalling bridge");
                        #[cfg(windows)]
                        let mut listener =
                            sapphire_ipc::bind(&ep).expect("bind the stalling bridge");
                        tokio::spawn(async move {
                            // Hold every connection open and answer nothing.
                            let mut held = Vec::new();
                            while let Ok(conn) = listener.accept().await {
                                held.push(conn);
                            }
                        });
                    });
                    let _ = rt_tx.send(rt);
                })
                .expect("spawn the stalling bridge thread");
            let rt = rt_rx
                .recv()
                .expect("the stalling bridge bound its endpoint");
            StallingBridge {
                endpoint,
                rt: Some(rt),
            }
        }
    }

    impl Drop for StallingBridge {
        fn drop(&mut self) {
            if let Some(rt) = self.rt.take() {
                rt.shutdown_background();
            }
        }
    }

    impl Drop for FakeBridge {
        fn drop(&mut self) {
            // `shutdown_background`, not a plain drop: a test may drop this inside a runtime.
            if let Some(rt) = self.rt.take() {
                rt.shutdown_background();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeBridge, StallingBridge};
    use super::*;
    use sapphire_bridge_api::EmbedInfoResult;

    #[test]
    fn connect_returns_none_when_no_bridge_listens() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::in_dir("bridge", tmp.path().to_path_buf());
        assert!(BridgeEmbedder::connect(Some(endpoint)).is_none());
    }

    #[test]
    fn connect_returns_none_when_embedding_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = FakeBridge::start(tmp.path(), EmbedInfoResult::default());
        assert!(BridgeEmbedder::connect(Some(bridge.endpoint.clone())).is_none());
    }

    #[test]
    fn embed_texts_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = FakeBridge::enabled(tmp.path());
        let (embedder, info) =
            BridgeEmbedder::connect(Some(bridge.endpoint.clone())).expect("embedding is enabled");
        assert_eq!(info.model, "fake-2d");
        assert_eq!(info.dimension, 2);

        let vectors = embedder.embed_texts(&["a", "bbb", "cc"]).unwrap();

        assert_eq!(
            vectors,
            vec![vec![0.0, 1.0], vec![1.0, 3.0], vec![2.0, 2.0]],
            "one 2-d vector per text, in order"
        );
        assert!(embedder.embed_texts(&[]).unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embed_texts_works_from_inside_a_tokio_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = FakeBridge::enabled(tmp.path());
        let (embedder, _) =
            BridgeEmbedder::connect(Some(bridge.endpoint.clone())).expect("embedding is enabled");

        // No `spawn_blocking`: the embedder's runtime is its own, so this must not panic.
        let vectors = embedder.embed_texts(&["x"]).unwrap();

        assert_eq!(vectors, vec![vec![0.0, 1.0]]);
        drop(embedder);
        drop(bridge);
    }

    /// A bridge that accepts the connection and then stalls must not hang `connect`: the
    /// `embed.info` probe is bounded, and a timeout is "no embedder", the same path as a
    /// failed connection.
    #[test]
    fn connect_gives_up_on_a_bridge_that_accepts_but_never_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = StallingBridge::start(tmp.path());

        let began = std::time::Instant::now();
        let connected = BridgeEmbedder::connect(Some(bridge.endpoint.clone()));
        let elapsed = began.elapsed();

        assert!(
            connected.is_none(),
            "a bridge that never answers embed.info is no embedder"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "connect must time out rather than hang: took {elapsed:?}"
        );
    }

    #[test]
    fn a_vector_from_another_model_is_an_error_until_the_embedder_follows() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = FakeBridge::enabled(tmp.path());
        let (embedder, _) =
            BridgeEmbedder::connect(Some(bridge.endpoint.clone())).expect("embedding is enabled");
        let other = EmbedModelInfo {
            model: "other-3d".into(),
            dimension: 3,
            template_version: 0,
            revision: None,
            max_tokens: None,
        };
        bridge.set_info(EmbedInfoResult {
            enabled: true,
            model: Some(other.clone()),
            ..EmbedInfoResult::default()
        });

        let err = embedder.embed_texts(&["x"]).unwrap_err();
        assert!(
            err.to_string().contains("now embeds with other-3d"),
            "{err}"
        );

        // Asking again on the same connection sees the new model; once accepted, it embeds.
        assert_eq!(embedder.current().unwrap(), Some(other.clone()));
        embedder.set_info(other);
        assert_eq!(
            embedder.embed_texts(&["x"]).unwrap(),
            vec![vec![0.0, 1.0, 0.0]]
        );

        bridge.set_info(EmbedInfoResult::default());
        assert_eq!(embedder.current().unwrap(), None, "embedding switched off");
    }

    #[test]
    fn embed_texts_fails_once_the_bridge_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = FakeBridge::enabled(tmp.path());
        let (embedder, _) =
            BridgeEmbedder::connect(Some(bridge.endpoint.clone())).expect("embedding is enabled");
        bridge.stop();

        assert!(embedder.embed_texts(&["x"]).is_err());
    }
}
