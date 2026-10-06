//! Integration tests for the IPC layer.

/// The crate must stay free of the workspace, search and sync stacks so that the bridge
/// can use it (spec §2, "the crate carries transport, framing, the router and process
/// startup"). Cargo enforces this; this test states it so a future dependency addition is
/// a deliberate act.
#[test]
fn dependency_surface_is_documented() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in [
        "sapphire-framework-workspace",
        "sapphire-framework-retrieve",
        "sapphire-framework-backend",
        "sapphire-framework-sync",
        "iroh",
        "reqwest",
        "axum",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "sapphire-framework-ipc must not depend on {forbidden}"
        );
    }
}

use std::sync::Arc;

use sapphire_framework_ipc::{
    Client, ClientInfo, Connection, Endpoint, ManagedBy, Router, ServerInfo, serve,
};

fn client_info() -> ClientInfo {
    ClientInfo {
        kind: "cli".into(),
        version: "0.0.0".into(),
        api: 1,
        pid: std::process::id(),
    }
}

fn server_info() -> ServerInfo {
    ServerInfo {
        version: "0.0.0".into(),
        api: 1,
        pid: std::process::id(),
        managed_by: ManagedBy::Spawned,
    }
}

fn router() -> Arc<Router> {
    Arc::new(
        Router::new()
            .method("echo", |ctx| async move { Ok(ctx.params) })
            .method("announce", |ctx| async move {
                ctx.peer
                    .notify("tick", serde_json::json!({ "n": 1 }))
                    .await
                    .ok();
                Ok(serde_json::json!(null))
            }),
    )
}

/// The in-process carrier: what mobile uses (spec §2.1).
#[tokio::test]
async fn in_process_round_trip() {
    let (client_conn, server_conn) = Connection::pair();
    tokio::spawn(async move {
        let _ = serve(server_conn, router(), "test-app", server_info()).await;
    });
    let (client, _) = Client::handshake(client_conn, "test-app", client_info())
        .await
        .unwrap();

    let echoed: serde_json::Value = client
        .call("echo", serde_json::json!({ "hello": "world" }))
        .await
        .unwrap();
    assert_eq!(echoed, serde_json::json!({ "hello": "world" }));
}

/// A large payload travels as a JSON string (spec §2.2).
#[tokio::test]
async fn a_multi_megabyte_payload_round_trips() {
    let (client_conn, server_conn) = Connection::pair();
    tokio::spawn(async move {
        let _ = serve(server_conn, router(), "test-app", server_info()).await;
    });
    let (client, _) = Client::handshake(client_conn, "test-app", client_info())
        .await
        .unwrap();

    let big = "y".repeat(4 * 1024 * 1024);
    let echoed: serde_json::Value = client
        .call("echo", serde_json::json!({ "content": big }))
        .await
        .unwrap();
    assert_eq!(echoed["content"].as_str().unwrap().len(), 4 * 1024 * 1024);
}

/// Notifications reach every subscriber of a connection.
#[tokio::test]
async fn two_subscribers_both_see_a_notification() {
    let (client_conn, server_conn) = Connection::pair();
    tokio::spawn(async move {
        let _ = serve(server_conn, router(), "test-app", server_info()).await;
    });
    let (client, _) = Client::handshake(client_conn, "test-app", client_info())
        .await
        .unwrap();

    let mut a = client.notifications();
    let mut b = client.notifications();
    let _: serde_json::Value = client
        .call("announce", serde_json::Value::Null)
        .await
        .unwrap();

    for events in [&mut a, &mut b] {
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n.method, "tick");
    }
}

/// The real carrier for this platform, end to end through the listener.
#[tokio::test]
async fn socket_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::in_dir("roundtrip-app", tmp.path().to_path_buf());

    #[cfg(unix)]
    let listener = sapphire_framework_ipc::bind(&endpoint).await.unwrap();
    #[cfg(windows)]
    let mut listener = sapphire_framework_ipc::bind(&endpoint).unwrap();

    tokio::spawn(async move {
        loop {
            let Ok(conn) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = serve(conn, router(), "roundtrip-app", server_info()).await;
            });
        }
    });

    let conn = sapphire_framework_ipc::connect(&endpoint).await.unwrap();
    let (client, info) = Client::handshake(conn, "roundtrip-app", client_info())
        .await
        .unwrap();
    assert_eq!(info.managed_by, ManagedBy::Spawned);

    let echoed: serde_json::Value = client
        .call("echo", serde_json::json!([1, 2, 3]))
        .await
        .unwrap();
    assert_eq!(echoed, serde_json::json!([1, 2, 3]));
}

/// Two clients on separate connections are served at the same time — the property the
/// whole design exists for: one server, several callers, no lock contention.
#[tokio::test(flavor = "multi_thread")]
async fn two_clients_are_served_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::in_dir("concurrent-app", tmp.path().to_path_buf());

    #[cfg(unix)]
    let listener = sapphire_framework_ipc::bind(&endpoint).await.unwrap();
    #[cfg(windows)]
    let mut listener = sapphire_framework_ipc::bind(&endpoint).unwrap();

    tokio::spawn(async move {
        loop {
            let Ok(conn) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = serve(conn, router(), "concurrent-app", server_info()).await;
            });
        }
    });

    let mut tasks = Vec::new();
    for n in 0..2u32 {
        let endpoint = endpoint.clone();
        tasks.push(tokio::spawn(async move {
            let conn = sapphire_framework_ipc::connect(&endpoint).await.unwrap();
            let (client, _) = Client::handshake(conn, "concurrent-app", client_info())
                .await
                .unwrap();
            let echoed: u32 = client.call("echo", n).await.unwrap();
            echoed
        }));
    }

    let mut seen = Vec::new();
    for task in tasks {
        seen.push(task.await.unwrap());
    }
    seen.sort_unstable();
    assert_eq!(seen, vec![0, 1]);
}

/// A call made *after* the server has gone must fail, not wait forever.
///
/// This is the socket version of `client`'s in-process test, and it is the one that pins the
/// real bug: an in-process carrier fails the `send` because the peer's receiver drops, while
/// a socket buffers the write, so the request is accepted and then nothing ever answers.
/// That is exactly the shape of a bridge that died while an app server was still talking to
/// it — the app server's next call hung for good.
#[tokio::test]
async fn a_call_made_after_the_server_has_gone_fails_rather_than_hanging() {
    let tmp = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::in_dir("gone-app", tmp.path().to_path_buf());

    #[cfg(unix)]
    let listener = sapphire_framework_ipc::bind(&endpoint).await.unwrap();
    #[cfg(windows)]
    let mut listener = sapphire_framework_ipc::bind(&endpoint).unwrap();

    // Served inline rather than in a spawned task, so that stopping this task drops the
    // connection — which is what a dying process does, and the whole point of the test.
    let server = tokio::spawn(async move {
        while let Ok(conn) = listener.accept().await {
            let _ = serve(conn, router(), "gone-app", server_info()).await;
        }
    });

    let conn = sapphire_framework_ipc::connect(&endpoint).await.unwrap();
    let (client, _) = Client::handshake(conn, "gone-app", client_info())
        .await
        .unwrap();

    // Still alive: this much must work, or the test is not about "after".
    let echoed: serde_json::Value = client.call("echo", serde_json::json!(1)).await.unwrap();
    assert_eq!(echoed, serde_json::json!(1));

    // The server goes away, and the client's reader has seen the close by the time the next
    // call starts.
    server.abort();
    let _ = server.await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.call::<_, serde_json::Value>("echo", serde_json::json!(2)),
    )
    .await
    .expect("a call on a closed connection must fail, not wait forever")
    .unwrap_err();
    assert!(
        matches!(err, sapphire_framework_ipc::Error::Closed),
        "got {err:?}"
    );
}

/// A server of another API version cannot be connected to and cannot be replaced, so the
/// handshake answers with [`Error::ApiVersionMismatch`], whose advice is to upgrade and
/// restart — this crate no longer retires anything, and a mismatch is the caller's to
/// resolve (migrated from the deleted `race.rs`).
#[tokio::test]
async fn a_server_of_another_api_version_is_reported_not_replaced() {
    let (client_conn, server_conn) = Connection::pair();
    let wrong = ServerInfo {
        api: 2,
        ..server_info()
    };
    tokio::spawn(async move {
        let _ = serve(server_conn, router(), "test-app", wrong).await;
    });

    let err = Client::handshake(client_conn, "test-app", client_info())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            sapphire_framework_ipc::Error::ApiVersionMismatch {
                running: 2,
                ours: 1,
                ..
            }
        ),
        "got {err:?}"
    );
    assert!(
        err.to_string().contains("restart the service"),
        "the error must carry the advice: {err}"
    );
}

/// The crate versions are not compared: an app server talks to a bridge built from
/// another crate, and their versions never line up. Only the API has to agree.
#[tokio::test]
async fn a_server_of_another_crate_version_but_the_same_api_is_connected_to() {
    let (client_conn, server_conn) = Connection::pair();
    let other = ServerInfo {
        version: "9.9.9".into(),
        ..server_info()
    };
    tokio::spawn(async move {
        let _ = serve(server_conn, router(), "test-app", other).await;
    });

    let (_client, info) = Client::handshake(client_conn, "test-app", client_info())
        .await
        .expect("the same API is enough");
    assert_eq!(info.version, "9.9.9");
}

/// A server built before API versions were exchanged sends no `api`; it spoke
/// [`FIRST_API`], so a client expecting that still talks to it without a restart.
#[test]
fn a_server_info_without_an_api_speaks_the_first_api() {
    let info: ServerInfo = serde_json::from_value(serde_json::json!({
        "version": "0.14.0",
        "pid": 1,
        "managed_by": "service",
    }))
    .unwrap();
    assert_eq!(info.api, sapphire_framework_ipc::FIRST_API);
}
