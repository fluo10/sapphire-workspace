//! The calling side of a connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, oneshot};

use crate::conn::{Connection, Sender};
use crate::error::{Error, Result};
use crate::handshake::{ClientInfo, Hello, ServerInfo, Welcome};
use crate::message::{Message, Notification, Request, ResponsePayload, RpcError};
use crate::router::HANDSHAKE_METHOD;

/// How many notifications may queue for a subscriber before it starts losing the oldest.
pub const NOTIFICATION_CAPACITY: usize = 256;

/// The calls in flight, and whether the connection has closed under them.
///
/// The two travel together under **one** lock, and that is the point. Closed-ness has to be
/// read and a call registered as a single step: were they separate, a call could see an open
/// connection, register itself, and then have the reader drain the map and exit before that
/// registration — leaving a caller waiting on an answer nothing will ever send. That is not
/// hypothetical: it is what an app server did to `sync.status` every time its bridge died.
#[derive(Debug, Default)]
struct Shared {
    closed: bool,
    pending: HashMap<u64, oneshot::Sender<std::result::Result<serde_json::Value, RpcError>>>,
}

impl Shared {
    /// Register a call, or refuse it because the connection is already closed.
    ///
    /// Refusing here is what makes a call on a dead connection fail promptly instead of
    /// hanging: nothing is left to answer it, and no later event will arrive to notice.
    fn register(
        &mut self,
        id: u64,
        tx: oneshot::Sender<std::result::Result<serde_json::Value, RpcError>>,
    ) -> Result<()> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.pending.insert(id, tx);
        Ok(())
    }

    /// Forget a call whose request could not be sent.
    fn forget(&mut self, id: u64) {
        self.pending.remove(&id);
    }

    /// Deliver a response, if anyone is still waiting for it.
    fn resolve(&mut self, id: u64, payload: std::result::Result<serde_json::Value, RpcError>) {
        if let Some(tx) = self.pending.remove(&id) {
            let _ = tx.send(payload);
        } else {
            tracing::debug!(id, "response for an unknown request");
        }
    }

    /// The connection is gone: mark it closed and fail everything still in flight.
    ///
    /// Closing and draining under the same lock is what leaves no gap — no call can slip in
    /// between the two and survive.
    fn close(&mut self) {
        self.closed = true;
        self.pending.clear();
    }
}

/// A connected client.
///
/// Cloning is not provided; share it behind an `Arc`. All methods take `&self`, so one
/// `Arc<Client>` serves any number of concurrent callers.
#[derive(Debug)]
pub struct Client {
    sender: Sender,
    shared: Arc<Mutex<Shared>>,
    events: broadcast::Sender<Notification>,
    next_id: AtomicU64,
    server: ServerInfo,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Client {
    /// Perform the handshake on an established connection.
    pub async fn handshake(
        mut conn: Connection,
        app: &str,
        client: ClientInfo,
    ) -> Result<(Client, ServerInfo)> {
        // Remembered here so the API gate below can name both sides; the moved-out
        // `client` goes into the hello.
        let ours = client.api;
        let ours_app = client.app_api;
        let sender = conn.sender();
        let hello = Hello {
            protocol: crate::PROTOCOL_VERSION,
            app: app.to_owned(),
            client,
        };
        sender
            .send(Message::Request(Request {
                id: 0,
                method: HANDSHAKE_METHOD.to_owned(),
                params: serde_json::to_value(hello)?,
            }))
            .await?;

        let welcome: Welcome = loop {
            match conn.recv().await {
                None => return Err(Error::Closed),
                Some(Err(err)) => return Err(err),
                Some(Ok(Message::Response(resp))) if resp.id == 0 => match resp.payload {
                    ResponsePayload::Ok(value) => break serde_json::from_value(value)?,
                    ResponsePayload::Err(err) => return Err(Error::Rpc(err)),
                },
                Some(Ok(_)) => continue,
            }
        };

        if welcome.protocol != crate::PROTOCOL_VERSION {
            return Err(Error::VersionMismatch {
                ours: crate::PROTOCOL_VERSION,
                theirs: welcome.protocol,
            });
        }

        // The API gate. The crate versions are not compared: the two ends are often
        // different crates (an app server and the bridge) whose versions never line up,
        // and a release that leaves the method set alone must not lock its peers out.
        // Nothing replaces a server any more, so every mismatch is an error.
        if welcome.server.api != ours {
            return Err(Error::ApiVersionMismatch {
                running: welcome.server.api,
                ours,
                server_version: welcome.server.version,
            });
        }
        // The application's own API, gated the same way, when the client calls it. A
        // server that reports none does not serve the version the client needs either.
        if let Some(ours) = ours_app
            && welcome.server.app_api != Some(ours)
        {
            return Err(Error::AppApiVersionMismatch {
                running: welcome.server.app_api,
                ours,
                server_version: welcome.server.version,
            });
        }

        let shared: Arc<Mutex<Shared>> = Arc::new(Mutex::new(Shared::default()));
        let (events, _) = broadcast::channel(NOTIFICATION_CAPACITY);

        let reader = tokio::spawn({
            let shared = Arc::clone(&shared);
            let events = events.clone();
            async move {
                let mut conn = conn;
                while let Some(incoming) = conn.recv().await {
                    match incoming {
                        Ok(Message::Response(resp)) => {
                            let payload = match resp.payload {
                                ResponsePayload::Ok(v) => Ok(v),
                                ResponsePayload::Err(e) => Err(e),
                            };
                            shared
                                .lock()
                                .expect("shared mutex")
                                .resolve(resp.id, payload);
                        }
                        Ok(Message::Notification(n)) => {
                            let _ = events.send(n);
                        }
                        Ok(Message::Request(req)) => {
                            tracing::debug!(method = %req.method, "ignoring a request from a server");
                        }
                        Err(err) => tracing::debug!("dropping a bad frame: {err}"),
                    }
                }
                // The connection closed: mark it, so a call starting from here is refused,
                // and fail every call still in flight rather than leaving callers waiting
                // forever. Both happen under one lock; see [`Shared`].
                shared.lock().expect("shared mutex").close();
            }
        });

        let server = welcome.server.clone();
        let client = Client {
            sender,
            shared,
            events,
            next_id: AtomicU64::new(1),
            server: welcome.server,
            reader,
        };
        Ok((client, server))
    }

    /// Call a method and deserialise its result.
    pub async fn call<P, R>(&self, method: &str, params: P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        // Refuses when the connection is already closed, rather than registering a caller
        // nothing will answer.
        self.shared.lock().expect("shared mutex").register(id, tx)?;

        let send = self
            .sender
            .send(Message::Request(Request {
                id,
                method: method.to_owned(),
                params: serde_json::to_value(params)?,
            }))
            .await;
        if let Err(err) = send {
            self.shared.lock().expect("shared mutex").forget(id);
            return Err(err);
        }

        match rx.await {
            Ok(Ok(value)) => Ok(serde_json::from_value(value)?),
            Ok(Err(err)) => Err(Error::Rpc(err)),
            Err(_) => Err(Error::Closed),
        }
    }

    /// Subscribe to server notifications. Each subscriber gets its own receiver, and one
    /// that falls [`NOTIFICATION_CAPACITY`] behind observes a lag rather than blocking the
    /// reader.
    pub fn notifications(&self) -> broadcast::Receiver<Notification> {
        self.events.subscribe()
    }

    /// What the server said about itself during the handshake.
    pub fn server(&self) -> &ServerInfo {
        &self.server
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::ManagedBy;
    use crate::router::{Router, serve};
    use serde_json::json;
    use std::sync::Arc;

    fn client_info() -> ClientInfo {
        ClientInfo {
            kind: "cli".into(),
            version: "0.0.0".into(),
            api: 1,
            app_api: None,
            pid: std::process::id(),
        }
    }

    fn server_info() -> ServerInfo {
        ServerInfo {
            version: "0.0.0".into(),
            api: 1,
            app_api: None,
            pid: 1,
            managed_by: ManagedBy::Spawned,
        }
    }

    async fn connect() -> (Client, ServerInfo) {
        let (client_conn, server_conn) = Connection::pair();
        let router = Arc::new(
            Router::new()
                .method("add", |ctx| async move {
                    let a = ctx.params.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                    let b = ctx.params.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                    Ok(json!(a + b))
                })
                .method("slow", |ctx| async move {
                    let ms = ctx.params.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    Ok(json!(ms))
                })
                .method("boom", |_| async move { Err(RpcError::internal("nope")) })
                .method("announce", |ctx| async move {
                    ctx.peer.notify("tick", json!({ "n": 1 })).await.ok();
                    Ok(json!(null))
                }),
        );
        tokio::spawn(async move {
            let _ = serve(server_conn, router, "test-app", server_info()).await;
        });
        Client::handshake(client_conn, "test-app", client_info())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_call_returns_a_typed_result() {
        let (client, info) = connect().await;
        assert_eq!(info.managed_by, ManagedBy::Spawned);
        let sum: i64 = client.call("add", json!({ "a": 2, "b": 3 })).await.unwrap();
        assert_eq!(sum, 5);
    }

    #[tokio::test]
    async fn calls_in_flight_together_are_matched_by_id() {
        let (client, _) = connect().await;
        let slow = client.call::<_, u64>("slow", json!({ "ms": 200 }));
        let quick = client.call::<_, i64>("add", json!({ "a": 1, "b": 1 }));
        let (slow, quick) = tokio::join!(slow, quick);
        assert_eq!(slow.unwrap(), 200);
        assert_eq!(quick.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_remote_error_surfaces_as_an_rpc_error() {
        let (client, _) = connect().await;
        let err = client
            .call::<_, serde_json::Value>("boom", json!(null))
            .await
            .unwrap_err();
        match err {
            Error::Rpc(e) => assert_eq!(e.message, "nope"),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notifications_reach_a_subscriber() {
        let (client, _) = connect().await;
        let mut events = client.notifications();
        let _: serde_json::Value = client.call("announce", json!(null)).await.unwrap();
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n.method, "tick");
    }

    #[tokio::test]
    async fn a_pending_call_fails_when_the_server_goes_away() {
        let (client_conn, server_conn) = Connection::pair();
        let router = Arc::new(Router::new().method("slow", |_| async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(json!(null))
        }));
        let server = tokio::spawn(async move {
            let _ = serve(server_conn, router, "test-app", server_info()).await;
        });
        let (client, _) = Client::handshake(client_conn, "test-app", client_info())
            .await
            .unwrap();

        let pending = tokio::spawn(async move {
            client
                .call::<_, serde_json::Value>("slow", json!(null))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        server.abort();

        let err = pending.await.unwrap().unwrap_err();
        assert!(matches!(err, Error::Closed), "got {err:?}");
    }

    #[tokio::test]
    async fn a_refused_handshake_is_reported() {
        let (client_conn, server_conn) = Connection::pair();
        let router = Arc::new(Router::new());
        tokio::spawn(async move {
            let _ = serve(server_conn, router, "other-app", server_info()).await;
        });
        let err = Client::handshake(client_conn, "test-app", client_info())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Rpc(_)), "got {err:?}");
    }
}
