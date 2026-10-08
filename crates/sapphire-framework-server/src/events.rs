//! `workspace.subscribe` and the pump that turns backend events into notifications.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sapphire_backend::protocol as proto;
use sapphire_backend::{WorkspaceBackend, protocol::Ack};
use sapphire_ipc::{PeerHandle, Router, RpcError};
use tokio::sync::broadcast::error::RecvError;

use crate::handlers::rpc_error;
use crate::host::WorkspaceHost;

/// Add `workspace.subscribe` to `router`.
///
/// The method opens the named workspace, subscribes to its
/// [`BackendEvent`](sapphire_backend::BackendEvent) broadcast and spawns a task that
/// forwards each event to that client as a `workspace.event` notification. The task stops
/// when the client disconnects — `PeerHandle::notify` fails once the connection is gone —
/// so a client that comes and goes does not leak tasks.
///
/// Subscribing twice from the same connection to the same workspace still acknowledges,
/// but starts no second pump, so a client cannot double its own event stream.
///
/// A subscriber that falls more than the backend's event capacity (128) behind loses the
/// events in between and gets a warning in the server log: treat an event as "something
/// happened, re-read", not as the change itself.
pub fn subscribe_method(host: Arc<WorkspaceHost>, router: Router) -> Router {
    // Which (client, workspace) pairs already have a pump. A client that subscribes twice
    // must not receive two copies of every event.
    let active: Arc<Mutex<HashSet<(u32, PathBuf)>>> = Arc::new(Mutex::new(HashSet::new()));

    router.method(proto::SUBSCRIBE, move |ctx| {
        let host = Arc::clone(&host);
        let active = Arc::clone(&active);
        async move {
            let p: proto::WsParams = serde_json::from_value(ctx.params.clone())
                .map_err(|e| RpcError::invalid_params(format!("bad parameters: {e}")))?;
            let backend = host.backend(&p.ws).await.map_err(|e| rpc_error(&e))?;

            let key = (ctx.peer.client().pid, p.ws.clone());
            if !active.lock().expect("subscription set").insert(key.clone()) {
                // Already pumping for this client and workspace.
                return serde_json::to_value(Ack {}).map_err(|e| RpcError::internal(e.to_string()));
            }

            let mut events = backend.subscribe();
            let peer: PeerHandle = ctx.peer.clone();
            let ws = p.ws.clone();
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let params = proto::EventParams {
                                ws: ws.clone(),
                                event,
                            };
                            let Ok(value) = serde_json::to_value(params) else {
                                continue;
                            };
                            // A failure here means the client is gone.
                            if peer.notify(proto::EVENT, value).await.is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(missed)) => {
                            tracing::warn!(
                                missed,
                                workspace = %ws.display(),
                                "a subscriber fell behind and lost events"
                            );
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
                active.lock().expect("subscription set").remove(&key);
            });

            serde_json::to_value(Ack {}).map_err(|e| RpcError::internal(e.to_string()))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_backend::BackendEvent;
    use sapphire_backend::protocol as proto;
    use sapphire_ipc::{Client, ClientInfo, Connection, ManagedBy, ServerInfo, serve};
    use sapphire_workspace::{AppContext, AppKind};
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::test_support;

    static CTX: AppContext = AppContext::new("sapphire-eventtest");

    /// The env vars `init_ctx` writes, in the order the guard restores them.
    const DIR_VARS: [&str; 3] = [
        "SAPPHIRE_EVENTTEST_CACHE_DIR",
        "SAPPHIRE_EVENTTEST_DATA_DIR",
        "SAPPHIRE_EVENTTEST_CONFIG_DIR",
    ];

    /// Point the context's directories at `tmp` and restore the previous values (present
    /// or absent) when dropped — including while unwinding from a panic. Without the
    /// unconditional restore, a panicking assertion would leave the variables pointing at
    /// a temp directory the test then deletes, and the context of a later test would
    /// resolve into the void.
    struct EnvGuard {
        previous: [Option<OsString>; 3],
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        /// Write the three directory vars while holding the environment lock.
        fn set(lock: std::sync::MutexGuard<'static, ()>, tmp: &std::path::Path) -> EnvGuard {
            let previous = DIR_VARS.map(std::env::var_os);
            let dirs = ["cache", "data", "config"].map(|cat| tmp.join(cat));
            // SAFETY (via `test_support::set`): `lock` serialises every read and write of
            // the process environment in this test binary — the `host` and `handlers`
            // modules' tests share the same lock — and it is held until `drop` has
            // restored the old values.
            for (name, dir) in DIR_VARS.iter().zip(dirs) {
                test_support::set(name, &dir);
            }
            EnvGuard {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: `self._lock` still serialises the environment; it is dropped only
            // after this method returns.
            for (name, previous) in DIR_VARS.iter().zip(self.previous.iter_mut()) {
                match previous.take() {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => test_support::remove(name),
                }
            }
        }
    }

    /// Point the context's directories at a scratch location. The static context is
    /// first-writer-wins, so whichever test initialises it first fixes the directories
    /// for the whole binary; every test therefore holds the env lock for its entire body
    /// (see [`Fixture`]).
    fn init_ctx(lock: std::sync::MutexGuard<'static, ()>, tmp: &std::path::Path) -> EnvGuard {
        let guard = EnvGuard::set(lock, tmp);
        CTX.init(AppKind::Server);
        guard
    }

    /// Everything one test needs, held for the test's whole body.
    ///
    /// `_tmp` is declared before `_env` so the scratch tree is deleted while the
    /// environment lock is still held: a sibling test that wakes on the lock must never
    /// observe the tree mid-deletion, nor env vars pointing at a deleted directory.
    struct Fixture {
        _tmp: tempfile::TempDir,
        _env: EnvGuard,
        ws: PathBuf,
        client: Client,
    }

    /// A server carrying `workspace.subscribe` over an in-process connection, plus a
    /// workspace root to use.
    ///
    /// The caller must keep the fixture (and with it the environment lock) alive for the
    /// whole test: the static [`CTX`] is first-writer-wins, so while one test runs, every
    /// other test that would re-point the context's directories must wait.
    async fn fixture() -> Fixture {
        let lock = test_support::lock();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(lock, tmp.path());
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(root.join(".sapphire-eventtest")).unwrap();
        let ws = root.canonicalize().unwrap();

        let host = Arc::new(WorkspaceHost::new(&CTX));
        let router = Arc::new(subscribe_method(
            Arc::clone(&host),
            crate::workspace_router(host),
        ));
        let (client_conn, server_conn) = Connection::pair();
        tokio::spawn(async move {
            let info = ServerInfo {
                version: "0.0.0".into(),
                api: sapphire_backend::protocol::API_VERSION,
                pid: std::process::id(),
                managed_by: ManagedBy::Spawned,
            };
            let _ = serve(server_conn, router, "sapphire-eventtest", info).await;
        });
        let client_info = ClientInfo {
            kind: "test".into(),
            version: "0.0.0".into(),
            api: sapphire_backend::protocol::API_VERSION,
            pid: std::process::id(),
        };
        let (client, _) = Client::handshake(client_conn, "sapphire-eventtest", client_info)
            .await
            .unwrap();
        Fixture {
            _tmp: tmp,
            _env,
            ws,
            client,
        }
    }

    #[tokio::test]
    async fn a_subscriber_sees_a_write_by_another_caller() {
        let f = fixture().await;
        let mut events = f.client.notifications();

        let _: proto::Ack = f
            .client
            .call(proto::SUBSCRIBE, proto::WsParams { ws: f.ws.clone() })
            .await
            .unwrap();
        let _: proto::Ack = f
            .client
            .call(
                proto::WRITE_FILE,
                proto::ContentParams {
                    ws: f.ws.clone(),
                    path: PathBuf::from("a.md"),
                    content: "x".into(),
                },
            )
            .await
            .unwrap();

        let notification = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event within five seconds")
            .unwrap();
        assert_eq!(notification.method, proto::EVENT);
        let params: proto::EventParams = serde_json::from_value(notification.params).unwrap();
        assert_eq!(params.ws, f.ws);
        assert!(
            matches!(params.event, BackendEvent::FileChanged { .. }),
            "{:?}",
            params.event
        );
    }

    #[tokio::test]
    async fn subscribing_to_a_directory_that_is_not_a_workspace_is_refused() {
        let f = fixture().await;
        let plain = f._tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        let err = f
            .client
            .call::<_, proto::Ack>(proto::SUBSCRIBE, proto::WsParams { ws: plain })
            .await
            .unwrap_err();
        assert!(matches!(err, sapphire_ipc::Error::Rpc(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn subscribing_twice_does_not_duplicate_events() {
        let f = fixture().await;
        let mut events = f.client.notifications();

        let _: proto::Ack = f
            .client
            .call(proto::SUBSCRIBE, proto::WsParams { ws: f.ws.clone() })
            .await
            .unwrap();
        let _: proto::Ack = f
            .client
            .call(proto::SUBSCRIBE, proto::WsParams { ws: f.ws.clone() })
            .await
            .unwrap();

        let _: proto::Ack = f
            .client
            .call(
                proto::WRITE_FILE,
                proto::ContentParams {
                    ws: f.ws.clone(),
                    path: PathBuf::from("a.md"),
                    content: "x".into(),
                },
            )
            .await
            .unwrap();

        // One event arrives …
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the first event")
            .unwrap();
        // … and no second copy of it.
        let second = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
        assert!(second.is_err(), "a duplicate event arrived: {second:?}");
    }
}
