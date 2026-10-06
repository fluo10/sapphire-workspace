//! Dispatching requests to handlers, and the per-connection server loop.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::task::JoinSet;

use serde_json::Value;

use crate::conn::{Connection, Sender};
use crate::error::Result;
use crate::handshake::{ClientInfo, Hello, ServerInfo, Welcome};
use crate::message::{Message, Notification, Request, Response, ResponsePayload, RpcError, codes};

/// The method name carrying the handshake (spec §2.4). It is a request like any other so
/// that one code path serves everything.
pub const HANDSHAKE_METHOD: &str = "ipc.hello";

/// What a handler returns.
pub type HandlerFuture = Pin<Box<dyn Future<Output = std::result::Result<Value, RpcError>> + Send>>;

type Handler = Arc<dyn Fn(RequestCtx) -> HandlerFuture + Send + Sync>;

/// A handle back to the client that made a request.
#[derive(Clone, Debug)]
pub struct PeerHandle {
    sender: Sender,
    client: Arc<ClientInfo>,
}

impl PeerHandle {
    /// Send a notification to this client.
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.sender
            .send(Message::Notification(Notification {
                method: method.to_owned(),
                params,
            }))
            .await
    }

    /// Who is on the other end.
    pub fn client(&self) -> &ClientInfo {
        &self.client
    }
}

/// Everything a handler is given.
#[derive(Debug)]
pub struct RequestCtx {
    /// The request's parameters.
    pub params: Value,
    /// The client that sent it.
    pub peer: PeerHandle,
}

/// Maps method names to handlers.
#[derive(Clone, Default)]
pub struct Router {
    methods: HashMap<String, Handler>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.methods.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("Router").field("methods", &names).finish()
    }
}

impl Router {
    /// An empty router.
    pub fn new() -> Router {
        Router::default()
    }

    /// Register `name`. Registering the same name twice replaces the first handler.
    pub fn method<F, Fut>(mut self, name: &str, f: F) -> Router
    where
        F: Fn(RequestCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<Value, RpcError>> + Send + 'static,
    {
        self.methods.insert(
            name.to_owned(),
            Arc::new(move |ctx| Box::pin(f(ctx)) as HandlerFuture),
        );
        self
    }

    /// Is `name` registered?
    pub fn has(&self, name: &str) -> bool {
        self.methods.contains_key(name)
    }

    fn get(&self, name: &str) -> Option<Handler> {
        self.methods.get(name).cloned()
    }
}

/// Serve one connection until it closes.
///
/// The first request must be [`HANDSHAKE_METHOD`]; anything else is refused. After that,
/// each request is dispatched on its own task, so a slow handler does not delay the
/// requests behind it. Dropping or aborting the `serve` future also drops the handlers
/// still running for that connection, which is what cancellation means here.
pub async fn serve(
    mut conn: Connection,
    router: Arc<Router>,
    app: &str,
    info: ServerInfo,
) -> Result<()> {
    let sender = conn.sender();
    let mut peer: Option<PeerHandle> = None;

    // Handlers are tracked, not detached, so a dropped `serve` call takes the
    // in-flight work down with it (cancellation is disconnection, spec §2.2).
    let mut handlers = JoinSet::new();

    while let Some(incoming) = conn.recv().await {
        // Reap the handlers that finished, so the set does not grow without bound.
        while handlers.try_join_next().is_some() {}
        let msg = match incoming {
            Ok(msg) => msg,
            Err(err) => {
                tracing::debug!("dropping a bad frame: {err}");
                continue;
            }
        };

        let Message::Request(req) = msg else {
            // Clients do not send notifications or responses to a server.
            tracing::debug!("ignoring a non-request frame from a client");
            continue;
        };

        if req.method == HANDSHAKE_METHOD {
            match handshake(&req, app, &info) {
                Ok((welcome, client)) => {
                    peer = Some(PeerHandle {
                        sender: sender.clone(),
                        client: Arc::new(client),
                    });
                    respond(&sender, req.id, ResponsePayload::Ok(welcome)).await?;
                }
                Err(err) => {
                    respond(&sender, req.id, ResponsePayload::Err(err)).await?;
                    break;
                }
            }
            continue;
        }

        let Some(peer) = peer.clone() else {
            respond(
                &sender,
                req.id,
                ResponsePayload::Err(RpcError {
                    code: codes::INVALID_REQUEST,
                    message: format!("the first request must be {HANDSHAKE_METHOD}"),
                    data: None,
                }),
            )
            .await?;
            continue;
        };

        let Some(handler) = router.get(&req.method) else {
            respond(
                &sender,
                req.id,
                ResponsePayload::Err(RpcError::method_not_found(&req.method)),
            )
            .await?;
            continue;
        };

        let sender = sender.clone();
        let Request { id, params, .. } = req;
        handlers.spawn(async move {
            let payload = match handler(RequestCtx { params, peer }).await {
                Ok(value) => ResponsePayload::Ok(value),
                Err(err) => ResponsePayload::Err(err),
            };
            let _ = respond(&sender, id, payload).await;
        });
    }

    Ok(())
}

/// Validate a handshake request, returning the welcome value to send back and the client
/// info to remember for the rest of the connection.
fn handshake(
    req: &Request,
    app: &str,
    info: &ServerInfo,
) -> std::result::Result<(Value, ClientInfo), RpcError> {
    let hello: Hello = serde_json::from_value(req.params.clone())
        .map_err(|e| RpcError::invalid_params(format!("malformed handshake: {e}")))?;
    if hello.protocol != crate::PROTOCOL_VERSION {
        return Err(RpcError {
            code: codes::INVALID_REQUEST,
            message: format!(
                "protocol version mismatch: this server speaks {}, the client speaks {}",
                crate::PROTOCOL_VERSION,
                hello.protocol
            ),
            data: Some(serde_json::json!({ "server_protocol": crate::PROTOCOL_VERSION })),
        });
    }
    if hello.app != app {
        return Err(RpcError {
            code: codes::INVALID_REQUEST,
            message: format!("this is the {app} server, not {}", hello.app),
            data: None,
        });
    }
    let welcome = Welcome {
        protocol: crate::PROTOCOL_VERSION,
        server: info.clone(),
    };
    let value = serde_json::to_value(welcome).map_err(|e| RpcError::internal(e.to_string()))?;
    Ok((value, hello.client))
}

async fn respond(sender: &Sender, id: u64, payload: ResponsePayload) -> Result<()> {
    sender
        .send(Message::Response(Response { id, payload }))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::{ClientInfo, Hello, ManagedBy};
    use crate::message::{Message, Request, ResponsePayload};
    use serde_json::json;
    use std::sync::Arc;

    fn info() -> ServerInfo {
        ServerInfo {
            version: "0.0.0".into(),
            api: 1,
            pid: 1,
            managed_by: ManagedBy::Spawned,
        }
    }

    fn hello() -> Message {
        Message::Request(Request {
            id: 0,
            method: HANDSHAKE_METHOD.into(),
            params: serde_json::to_value(Hello {
                protocol: crate::PROTOCOL_VERSION,
                app: "test-app".into(),
                client: ClientInfo {
                    kind: "cli".into(),
                    version: "0.0.0".into(),
                    api: 1,
                    pid: 2,
                },
            })
            .unwrap(),
        })
    }

    fn router() -> Arc<Router> {
        Arc::new(
            Router::new()
                .method("echo", |ctx| async move { Ok(ctx.params) })
                .method("slow", |ctx| async move {
                    let ms = ctx.params.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    Ok(json!("done"))
                })
                .method("boom", |_| async move { Err(RpcError::internal("nope")) })
                .method("announce", |ctx| async move {
                    ctx.peer.notify("event", json!({ "hi": true })).await.ok();
                    Ok(json!(null))
                }),
        )
    }

    async fn connected() -> Connection {
        let (client, server) = Connection::pair();
        let router = router();
        tokio::spawn(async move {
            let _ = serve(server, router, "test-app", info()).await;
        });
        let mut client = client;
        client.send(hello()).await.unwrap();
        let welcome = client.recv().await.unwrap().unwrap();
        match welcome {
            Message::Response(r) => assert!(matches!(r.payload, ResponsePayload::Ok(_))),
            other => panic!("expected a welcome, got {other:?}"),
        }
        client
    }

    async fn call(client: &mut Connection, id: u64, method: &str, params: serde_json::Value) {
        client
            .send(Message::Request(Request {
                id,
                method: method.into(),
                params,
            }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_request_gets_its_result() {
        let mut client = connected().await;
        call(&mut client, 1, "echo", json!({ "a": 1 })).await;
        match client.recv().await.unwrap().unwrap() {
            Message::Response(Response {
                id: 1,
                payload: ResponsePayload::Ok(v),
            }) => {
                assert_eq!(v, json!({ "a": 1 }));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_method_is_answered_with_method_not_found() {
        let mut client = connected().await;
        call(&mut client, 1, "nope", serde_json::Value::Null).await;
        match client.recv().await.unwrap().unwrap() {
            Message::Response(Response {
                payload: ResponsePayload::Err(e),
                ..
            }) => {
                assert_eq!(e.code, crate::codes::METHOD_NOT_FOUND);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_handler_is_answered_with_its_error() {
        let mut client = connected().await;
        call(&mut client, 9, "boom", serde_json::Value::Null).await;
        match client.recv().await.unwrap().unwrap() {
            Message::Response(Response {
                id: 9,
                payload: ResponsePayload::Err(e),
            }) => {
                assert_eq!(e.message, "nope");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_slow_request_does_not_block_the_one_behind_it() {
        let mut client = connected().await;
        call(&mut client, 1, "slow", json!({ "ms": 300 })).await;
        call(&mut client, 2, "echo", json!("quick")).await;

        let first = client.recv().await.unwrap().unwrap();
        match first {
            Message::Response(Response { id: 2, .. }) => {}
            other => panic!("the quick call should answer first, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_handler_can_notify_its_client() {
        let mut client = connected().await;
        call(&mut client, 1, "announce", serde_json::Value::Null).await;

        let mut saw_notification = false;
        let mut saw_response = false;
        for _ in 0..2 {
            match client.recv().await.unwrap().unwrap() {
                Message::Notification(n) => {
                    assert_eq!(n.method, "event");
                    saw_notification = true;
                }
                Message::Response(_) => saw_response = true,
                other => panic!("got {other:?}"),
            }
        }
        assert!(saw_notification && saw_response);
    }

    #[tokio::test]
    async fn a_wrong_protocol_version_is_refused() {
        let (client, server) = Connection::pair();
        tokio::spawn(async move {
            let _ = serve(server, router(), "test-app", info()).await;
        });
        let mut client = client;
        client
            .send(Message::Request(Request {
                id: 0,
                method: HANDSHAKE_METHOD.into(),
                params: json!({
                    "protocol": crate::PROTOCOL_VERSION + 1,
                    "app": "test-app",
                    "client": { "kind": "cli", "version": "0.0.0", "pid": 2 }
                }),
            }))
            .await
            .unwrap();
        match client.recv().await.unwrap().unwrap() {
            Message::Response(Response {
                payload: ResponsePayload::Err(e),
                ..
            }) => {
                assert!(e.message.contains("protocol"), "{}", e.message);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_request_before_the_handshake_is_refused() {
        let (client, server) = Connection::pair();
        tokio::spawn(async move {
            let _ = serve(server, router(), "test-app", info()).await;
        });
        let mut client = client;
        call(&mut client, 1, "echo", json!(1)).await;
        match client.recv().await.unwrap().unwrap() {
            Message::Response(Response {
                payload: ResponsePayload::Err(e),
                ..
            }) => {
                assert_eq!(e.code, crate::codes::INVALID_REQUEST);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_handshake_for_another_app_is_refused() {
        let (client, server) = Connection::pair();
        tokio::spawn(async move {
            let _ = serve(server, router(), "other-app", info()).await;
        });
        let mut client = client;
        client.send(hello()).await.unwrap();
        match client.recv().await.unwrap().unwrap() {
            Message::Response(Response {
                payload: ResponsePayload::Err(e),
                ..
            }) => {
                assert!(e.message.contains("other-app"), "{}", e.message);
            }
            other => panic!("got {other:?}"),
        }
    }
}
