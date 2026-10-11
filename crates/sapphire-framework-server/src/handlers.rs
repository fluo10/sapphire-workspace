//! The `workspace.*` methods, one per [`WorkspaceBackend`] method.

use std::sync::Arc;

use sapphire_backend::protocol as proto;
use sapphire_backend::{WorkspaceBackend, protocol::Ack};
use sapphire_ipc::{RequestCtx, Router, RpcError, codes};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::current::Current;
use crate::error::Error;
use crate::sync::SyncRuntime;

/// Turn a server error into a JSON-RPC error.
///
/// Only a caller's mistake is `INVALID_PARAMS`: a path outside the workspace, a directory
/// that is not a workspace, an unreadable sync id, a parameter object that does not fit. A
/// failing database or an unreachable bridge is the server's problem, and the caller cannot
/// fix it by asking differently.
pub(crate) fn rpc_error(err: &Error) -> RpcError {
    use sapphire_workspace::Error as WsError;

    let caller_error = matches!(
        err,
        Error::UnknownWorkspace(..)
            | Error::NoWorkspace
            | Error::UnknownWorkspaceName(..)
            | Error::WrongApp { .. }
            | Error::SyncId(..)
            | Error::Workspace(
                WsError::PathEscapesWorkspace { .. }
                    | WsError::MarkerDirMissing { .. }
                    | WsError::MarkerNotFound { .. }
            )
    ) || matches!(
        err,
        Error::Backend(sapphire_backend::Error::Workspace(
            WsError::PathEscapesWorkspace { .. }
                | WsError::MarkerDirMissing { .. }
                | WsError::MarkerNotFound { .. }
        ))
    );

    if caller_error {
        RpcError::invalid_params(err.to_string())
    } else {
        RpcError {
            code: codes::INTERNAL_ERROR,
            message: err.to_string(),
            data: None,
        }
    }
}

fn params<T: DeserializeOwned>(ctx: &RequestCtx) -> std::result::Result<T, RpcError> {
    serde_json::from_value(ctx.params.clone())
        .map_err(|e| RpcError::invalid_params(format!("bad parameters: {e}")))
}

fn ok<T: serde::Serialize>(value: T) -> std::result::Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(e.to_string()))
}

/// The file methods of `workspace.*`, on the current workspace (#215).
///
/// The three methods that change a file take the exact path: after the write succeeds, the
/// replica is told to [`scan`], so the change is committed without waiting for the watcher's
/// debounce. The watcher is the safety net for edits the server did not make; this is the
/// main route, and it is not optional for the same reason the watcher is not — a skipped
/// scan loses an edit until the next one.
///
/// [`scan`]: SyncRuntime::scan
pub fn workspace_router(current: Arc<Current>) -> Router {
    let read = Arc::clone(&current);
    let write = Arc::clone(&current);
    let append = Arc::clone(&current);
    let delete = Arc::clone(&current);
    let list = Arc::clone(&current);
    let search = Arc::clone(&current);
    let reindex = current;

    /// Scan `root` after a write, if this server syncs. A failed scan is logged, not
    /// returned: the caller's write succeeded, and reporting a sync problem as a write
    /// failure would make an unrelated outage look like the caller's.
    async fn scanned(sync: Option<&Arc<SyncRuntime>>, root: &std::path::Path, what: &str) {
        if let Some(runtime) = sync {
            if let Err(err) = runtime.scan(root).await {
                tracing::warn!(root = %root.display(), "scan after {what} failed: {err}");
            }
            runtime.request_embed(root);
        }
    }

    Router::new()
        .method(proto::READ_FILE, move |ctx| {
            let current = Arc::clone(&read);
            async move {
                let p: proto::PathParams = params(&ctx)?;
                let backend = current.host().backend().await.map_err(|e| rpc_error(&e))?;
                let content = backend
                    .read_file(&p.path)
                    .await
                    .map_err(|e| rpc_error(&Error::Backend(e)))?;
                ok(proto::ReadResult { content })
            }
        })
        .method(proto::WRITE_FILE, move |ctx| {
            let current = Arc::clone(&write);
            async move {
                let p: proto::ContentParams = params(&ctx)?;
                let (root, backend) = current.host().current().await.map_err(|e| rpc_error(&e))?;
                backend
                    .write_file(&p.path, &p.content)
                    .await
                    .map_err(|e| rpc_error(&Error::Backend(e)))?;
                scanned(current.sync(), &root, "a write").await;
                ok(Ack {})
            }
        })
        .method(proto::APPEND_FILE, move |ctx| {
            let current = Arc::clone(&append);
            async move {
                let p: proto::ContentParams = params(&ctx)?;
                let (root, backend) = current.host().current().await.map_err(|e| rpc_error(&e))?;
                backend
                    .append_file(&p.path, &p.content)
                    .await
                    .map_err(|e| rpc_error(&Error::Backend(e)))?;
                scanned(current.sync(), &root, "an append").await;
                ok(Ack {})
            }
        })
        .method(proto::DELETE_FILE, move |ctx| {
            let current = Arc::clone(&delete);
            async move {
                let p: proto::PathParams = params(&ctx)?;
                let (root, backend) = current.host().current().await.map_err(|e| rpc_error(&e))?;
                backend
                    .delete_file(&p.path)
                    .await
                    .map_err(|e| rpc_error(&Error::Backend(e)))?;
                scanned(current.sync(), &root, "a delete").await;
                ok(Ack {})
            }
        })
        .method(proto::LIST_DIR, move |ctx| {
            let current = Arc::clone(&list);
            async move {
                let p: proto::PathParams = params(&ctx)?;
                let backend = current.host().backend().await.map_err(|e| rpc_error(&e))?;
                let entries = backend
                    .list_dir(&p.path)
                    .await
                    .map_err(|e| rpc_error(&Error::Backend(e)))?
                    .into_iter()
                    .map(|(path, is_dir)| proto::DirEntry { path, is_dir })
                    .collect();
                ok(proto::ListDirResult { entries })
            }
        })
        .method(proto::SEARCH, move |ctx| {
            let current = Arc::clone(&search);
            async move {
                let p: proto::SearchParams = params(&ctx)?;
                let backend = current.host().backend().await.map_err(|e| rpc_error(&e))?;
                let hits = backend
                    .search(&p.query, p.limit, p.mode)
                    .await
                    .map_err(|e| rpc_error(&Error::Backend(e)))?;
                ok(proto::SearchResult { hits })
            }
        })
        .method(proto::REINDEX, move |_| {
            let current = Arc::clone(&reindex);
            async move {
                let backend = current.host().backend().await.map_err(|e| rpc_error(&e))?;
                let summary = backend
                    .sync()
                    .await
                    .map_err(|e| rpc_error(&Error::Backend(e)))?;
                ok(proto::ReindexResult {
                    upserted: summary.upserted,
                    removed: summary.removed,
                })
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_backend::protocol as proto;
    use sapphire_ipc::{Client, ClientInfo, Connection, ManagedBy, ServerInfo, serve};
    use sapphire_workspace::{AppContext, AppKind};
    use std::ffi::OsString;
    use std::path::PathBuf;

    use crate::test_support;

    static CTX: AppContext = AppContext::new("sapphire-handlertest");

    /// The env vars `init_ctx` writes, in the order the guard restores them.
    const DIR_VARS: [&str; 3] = [
        "SAPPHIRE_HANDLERTEST_CACHE_DIR",
        "SAPPHIRE_HANDLERTEST_DATA_DIR",
        "SAPPHIRE_HANDLERTEST_CONFIG_DIR",
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
            // the process environment in this test binary — the `host` module's tests
            // share the same lock — and it is held until `drop` has restored the old
            // values.
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

    /// A server over an in-process connection, plus a workspace root to use.
    ///
    /// The caller must keep the fixture (and with it the environment lock) alive for the
    /// whole test: the static [`CTX`] is first-writer-wins, so while one test runs, every
    /// other test that would re-point the context's directories must wait.
    async fn fixture() -> Fixture {
        fixture_selecting(true).await
    }

    async fn fixture_selecting(select: bool) -> Fixture {
        let lock = test_support::lock();
        let tmp = tempfile::tempdir().unwrap();
        let _env = init_ctx(lock, tmp.path());
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(root.join(".sapphire-handlertest")).unwrap();
        let ws = root.canonicalize().unwrap();

        let current = Arc::new(Current::new(
            &CTX,
            Arc::new(crate::host::WorkspaceHost::new(&CTX)),
            None,
            crate::selection::SelectionFile::at(tmp.path().join("workspace.toml")),
        ));
        if select {
            current.select(&ws).await.unwrap();
        }
        let router = Arc::new(workspace_router(current));
        let (client_conn, server_conn) = Connection::pair();
        tokio::spawn(async move {
            let info = ServerInfo {
                version: "0.0.0".into(),
                api: sapphire_backend::protocol::API_VERSION,
                app_api: None,
                pid: std::process::id(),
                managed_by: ManagedBy::Spawned,
            };
            let _ = serve(server_conn, router, "sapphire-handlertest", info).await;
        });
        let client_info = ClientInfo {
            kind: "test".into(),
            version: "0.0.0".into(),
            api: sapphire_backend::protocol::API_VERSION,
            app_api: None,
            pid: std::process::id(),
        };
        let (client, _) = Client::handshake(client_conn, "sapphire-handlertest", client_info)
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
    async fn a_file_written_through_the_server_can_be_read_back() {
        let f = fixture().await;
        let _: proto::Ack = f
            .client
            .call(
                proto::WRITE_FILE,
                proto::ContentParams {
                    path: PathBuf::from("a.md"),
                    content: "# hello".into(),
                },
            )
            .await
            .unwrap();

        let read: proto::ReadResult = f
            .client
            .call(
                proto::READ_FILE,
                proto::PathParams {
                    path: PathBuf::from("a.md"),
                },
            )
            .await
            .unwrap();
        assert_eq!(read.content, "# hello");
    }

    #[tokio::test]
    async fn appending_adds_to_the_file() {
        let f = fixture().await;
        let write = proto::ContentParams {
            path: PathBuf::from("a.md"),
            content: "one\n".into(),
        };
        let _: proto::Ack = f.client.call(proto::WRITE_FILE, write).await.unwrap();
        let append = proto::ContentParams {
            path: PathBuf::from("a.md"),
            content: "two\n".into(),
        };
        let _: proto::Ack = f.client.call(proto::APPEND_FILE, append).await.unwrap();

        let read: proto::ReadResult = f
            .client
            .call(
                proto::READ_FILE,
                proto::PathParams {
                    path: PathBuf::from("a.md"),
                },
            )
            .await
            .unwrap();
        assert_eq!(read.content, "one\ntwo\n");
    }

    #[tokio::test]
    async fn a_deleted_file_is_gone() {
        let f = fixture().await;
        let _: proto::Ack = f
            .client
            .call(
                proto::WRITE_FILE,
                proto::ContentParams {
                    path: PathBuf::from("a.md"),
                    content: "x".into(),
                },
            )
            .await
            .unwrap();
        let _: proto::Ack = f
            .client
            .call(
                proto::DELETE_FILE,
                proto::PathParams {
                    path: PathBuf::from("a.md"),
                },
            )
            .await
            .unwrap();

        let err = f
            .client
            .call::<_, proto::ReadResult>(
                proto::READ_FILE,
                proto::PathParams {
                    path: PathBuf::from("a.md"),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, sapphire_ipc::Error::Rpc(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_directory_listing_separates_files_from_directories() {
        let f = fixture().await;
        std::fs::create_dir_all(f.ws.join("notes")).unwrap();
        let _: proto::Ack = f
            .client
            .call(
                proto::WRITE_FILE,
                proto::ContentParams {
                    path: PathBuf::from("a.md"),
                    content: "x".into(),
                },
            )
            .await
            .unwrap();

        let listing: proto::ListDirResult = f
            .client
            .call(
                proto::LIST_DIR,
                proto::PathParams {
                    path: PathBuf::from("."),
                },
            )
            .await
            .unwrap();
        assert!(
            listing
                .entries
                .iter()
                .any(|e| e.is_dir && e.path.ends_with("notes")),
            "{:?}",
            listing.entries
        );
        assert!(
            listing
                .entries
                .iter()
                .any(|e| !e.is_dir && e.path.ends_with("a.md")),
            "{:?}",
            listing.entries
        );
    }

    #[tokio::test]
    async fn a_written_file_is_searchable() {
        let f = fixture().await;
        let _: proto::Ack = f
            .client
            .call(
                proto::WRITE_FILE,
                proto::ContentParams {
                    path: PathBuf::from("a.md"),
                    content: "the quick brown fox".into(),
                },
            )
            .await
            .unwrap();

        let hits: proto::SearchResult = f
            .client
            .call(
                proto::SEARCH,
                proto::SearchParams {
                    query: "brown".into(),
                    limit: 10,
                    mode: sapphire_backend::SearchMode::Fts,
                },
            )
            .await
            .unwrap();
        assert!(
            hits.hits.iter().any(|h| h.path.ends_with("a.md")),
            "{:?}",
            hits.hits
        );
    }

    #[tokio::test]
    async fn reindexing_reports_what_it_found() {
        let f = fixture().await;
        std::fs::write(f.ws.join("outside.md"), "written behind the server's back").unwrap();

        let report: proto::ReindexResult = f
            .client
            .call(proto::REINDEX, serde_json::json!({}))
            .await
            .unwrap();
        assert!(report.upserted >= 1, "{report:?}");
    }

    #[tokio::test]
    async fn a_path_escaping_the_workspace_is_an_invalid_parameter() {
        let f = fixture().await;
        let err = f
            .client
            .call::<_, proto::ReadResult>(
                proto::READ_FILE,
                proto::PathParams {
                    path: PathBuf::from("../../etc/passwd"),
                },
            )
            .await
            .unwrap_err();
        match err {
            sapphire_ipc::Error::Rpc(e) => {
                assert_eq!(e.code, sapphire_ipc::codes::INVALID_PARAMS, "{}", e.message);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn without_a_workspace_every_call_is_an_invalid_parameter() {
        let f = fixture_selecting(false).await;
        let err = f
            .client
            .call::<_, proto::ReadResult>(
                proto::READ_FILE,
                proto::PathParams {
                    path: PathBuf::from("a.md"),
                },
            )
            .await
            .unwrap_err();
        match err {
            sapphire_ipc::Error::Rpc(e) => {
                assert_eq!(e.code, sapphire_ipc::codes::INVALID_PARAMS, "{}", e.message);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_malformed_parameter_object_is_an_invalid_parameter() {
        let f = fixture().await;
        let err = f
            .client
            .call::<_, proto::ReadResult>(proto::READ_FILE, serde_json::json!({ "ws": 42 }))
            .await
            .unwrap_err();
        match err {
            sapphire_ipc::Error::Rpc(e) => {
                assert_eq!(e.code, sapphire_ipc::codes::INVALID_PARAMS);
            }
            other => panic!("got {other:?}"),
        }
    }
}
