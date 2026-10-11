//! The app server skeleton.
//!
//! An application builds one of these, adds its own methods, and runs it. Everything a
//! sapphire app needs on the server side — owning its workspace, answering `workspace.*`,
//! pushing events — is here. A server serves one workspace at a time (#215): see
//! [`Current`].
//!
//! See `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §4.
//!
//! ```rust,ignore
//! static CTX: AppContext = AppContext::new("sapphire-journal");
//!
//! AppServer::new(&CTX, env!("CARGO_PKG_VERSION"))
//!     .extend(|router| router.method("journal.create_entry", create_entry))
//!     .run()
//!     .await?;
//! ```
//!
//! Application methods are named `<app>.<method>`. The framework owns `workspace.*` and
//! `server.*`; anything else is the application's.

#![warn(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sapphire_backend::protocol as proto;
use sapphire_framework_service::ServiceSpec;
use sapphire_ipc::{Endpoint, ManagedBy, Router, ServerInfo, serve};
use sapphire_workspace::AppContext;

/// How long a server's shutdown waits for its open connections to wind down on their own.
///
/// The shutdown acknowledgement is one of those connections; a moment is all a well-behaved
/// client needs. Whatever still holds on after this is cancelled, so a connection kept open
/// past shutdown cannot pin the router — and through it the sync runtime's replica stores —
/// after the server has answered its own shutdown call.
const SERVE_GRACE: Duration = Duration::from_secs(2);

mod command;
mod current;
mod error;
mod events;
mod handlers;
mod host;
mod selection;
pub mod sync;
#[cfg(test)]
mod test_support;

pub use command::{
    DeviceCommand, FrameworkCommand, StatusReport, StatusRow, WorkgroupCommand, WorkspaceCommand,
    render_workspace,
};
pub use current::Current;
pub use error::{Error, Result};
pub use events::subscribe_method;
pub use handlers::workspace_router;
pub use host::WorkspaceHost;
pub use selection::{SELECTION_FILE, Selection, SelectionFile};
pub use sync::{BackfillTiming, SyncRuntime, SyncStatus, sync_router};

/// An application's server.
pub struct AppServer {
    ctx: &'static AppContext,
    version: &'static str,
    endpoint: Option<Endpoint>,
    /// Where `workspace.toml` lives; `None` is the config directory's.
    selection_file: Option<PathBuf>,
    host: Arc<WorkspaceHost>,
    sync: Option<Arc<SyncRuntime>>,
    extend: Option<Box<dyn FnOnce(Router) -> Router + Send>>,
    /// The application's own status rows, shown after the framework's in `status` and in
    /// the `server.info` report.
    status_rows: Option<Arc<dyn Fn() -> Vec<StatusRow> + Send + Sync>>,
}

/// The SIGTERM end of the server's select loop, in a shape every platform shares.
///
/// [`AppServer::run`]'s select arm awaits `recv()` on one of these, exactly as the
/// plan writes it for Unix. Unix backs it with the `SignalKind::terminate()`
/// stream; Windows has no SIGTERM, so there the future never resolves and the arm
/// exists for the macro's sake and never fires. Without the common shape the arm
/// would need a `#[cfg]` of its own, which tokio's `select!` rejects once the
/// attribute removes the arm.
#[cfg(unix)]
struct Sigterm(tokio::signal::unix::Signal);

/// The never-firing stand-in for the Unix `Sigterm` where there is no SIGTERM.
#[cfg(not(unix))]
struct Sigterm;

#[cfg(unix)]
impl Sigterm {
    /// Register interest in SIGTERM.
    ///
    /// Called before the listener binds, so a signal arriving the instant the
    /// socket is up is queued by tokio instead of killing the process with the
    /// default handler.
    fn new() -> Result<Self> {
        Ok(Self(tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )?))
    }

    /// Wait for the next SIGTERM.
    ///
    /// Never returns `None`: the stream is infinite, as its documentation states.
    async fn recv(&mut self) {
        self.0.recv().await;
    }
}

#[cfg(not(unix))]
impl Sigterm {
    /// A future that is never ready — the arm awaits it for the macro's sake only.
    ///
    /// There is no `new`: this platform has no SIGTERM to register interest in, so
    /// the loop binds the unit value directly.
    async fn recv(&mut self) {
        std::future::pending::<()>().await
    }
}

impl AppServer {
    /// A server for `ctx`'s application, reporting `version` in its handshake.
    pub fn new(ctx: &'static AppContext, version: &'static str) -> AppServer {
        AppServer {
            ctx,
            version,
            endpoint: None,
            selection_file: None,
            host: Arc::new(WorkspaceHost::new(ctx)),
            sync: None,
            extend: None,
            status_rows: None,
        }
    }

    /// The application this server serves, as its [`AppContext`] names it.
    ///
    /// [`FrameworkCommand`](crate::FrameworkCommand) reads it to build the CLI's endpoint,
    /// so an application never passes its own name twice.
    pub fn app_name(&self) -> &'static str {
        self.ctx.app_name
    }

    /// Listen somewhere other than the application's default endpoint. Used by tests.
    pub fn endpoint(mut self, endpoint: Endpoint) -> AppServer {
        self.endpoint = Some(endpoint);
        self
    }

    /// Keep the selected workspace (`workspace.toml`) at `path` rather than in the
    /// application's config directory: for fixtures that run several hosts in one process.
    pub fn selection_file(mut self, path: PathBuf) -> AppServer {
        self.selection_file = Some(path);
        self
    }

    /// Serve `sync.enable`, `sync.disable`, `sync.map` and `sync.status`, and keep a replica
    /// of the workspace in step with its files while it is synced.
    ///
    /// Without this the server has no sync at all: an application that does not call it
    /// never talks to the bridge. The runtime is built by the caller because it needs the
    /// bridge connection, which is the caller's to open and to close.
    pub fn sync(mut self, runtime: Arc<SyncRuntime>) -> AppServer {
        // The runtime re-indexes what a session writes, and the index belongs to this host:
        // tell it where the workspace lives before anything can arrive.
        runtime.set_host(Arc::clone(&self.host));
        self.sync = Some(runtime);
        self
    }

    /// The application's own rows for the status report: shown after the framework's
    /// `running` / `version` / `pid` / `managed_by` lines.
    ///
    /// The closure is called once per report, so an application's rows may read live
    /// state; the CLI's `status` and the IPC `server.info` method render the same call's
    /// output.
    pub fn status_rows(mut self, rows: Arc<dyn Fn() -> Vec<StatusRow> + Send + Sync>) -> AppServer {
        self.status_rows = Some(rows);
        self
    }

    /// What this application's service is: the arguments a service manager starts it with.
    ///
    /// `["serve"]`: a service manager starts the executable directly, and the executable's
    /// bare invocation is `serve` — the subcommand-less default of
    /// [`FrameworkCommand::Serve`](crate::FrameworkCommand::Serve). A CLI that wants its
    /// service installed hands this to
    /// [`ServiceCommand`](sapphire_framework_service::ServiceCommand).
    pub fn service_spec(&self) -> ServiceSpec {
        ServiceSpec {
            app_name: self.ctx.app_name,
            description: format!("{} server {}", self.ctx.app_name, self.version),
            args: vec!["serve".to_owned()],
            post_install: None,
        }
    }

    /// Add the application's own methods.
    pub fn extend(mut self, f: impl FnOnce(Router) -> Router + Send + 'static) -> AppServer {
        self.extend = Some(Box::new(f));
        self
    }

    /// The workspace this server has open. An application's own handlers use it to reach
    /// the workspace the same way the framework's do: [`WorkspaceHost::backend`].
    pub fn host(&self) -> &Arc<WorkspaceHost> {
        &self.host
    }

    /// Listen until told to stop, or until a signal arrives.
    pub async fn run(self) -> Result<()> {
        let AppServer {
            ctx,
            version,
            endpoint,
            selection_file,
            host,
            sync,
            extend,
            status_rows,
        } = self;

        let endpoint = match endpoint {
            Some(e) => e,
            None => Endpoint::for_app(ctx.app_name)?,
        };
        // A server is always-on from here on: only `serve` and the service manager start
        // one, and the service manager starts it for keeps. There is no spawned mode left
        // to report.
        let info = ServerInfo {
            version: version.to_owned(),
            api: proto::API_VERSION,
            pid: std::process::id(),
            managed_by: ManagedBy::Service,
        };

        // The app's log file, held for as long as this server serves: the guard's
        // drop flushes the writer thread, so the last lines of a clean shutdown land
        // in the file. The subscriber itself was installed by `ctx.init`; this is what
        // routes the file layer at `<data dir>/logs/`.
        let _log = sapphire_workspace::logging::install(ctx)?;

        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);

        let file = match selection_file {
            Some(path) => SelectionFile::at(path),
            None => SelectionFile::for_app(ctx),
        };
        let current = Arc::new(Current::new(ctx, Arc::clone(&host), sync.clone(), file));
        // Back to the workspace this host had, before the first client can ask for it.
        let restoring = current.restore().await;

        let mut router = subscribe_method(
            Arc::clone(&host),
            current::current_methods(Arc::clone(&current), workspace_router(Arc::clone(&current))),
        );
        if sync.is_some() {
            // `sync_router` is applied *under* the application's own methods so a later
            // `extend` can still replace one, and after `subscribe_method` so the
            // `workspace.*` namespace is complete.
            router = sync_router(Arc::clone(&current), router);
        }
        // `server.info` answers the typed [`StatusReport`] — the same shape the CLI's
        // `status` renders — so the CLI and a future GUI read one record. The rows come
        // from the application's builder, called once per report.
        router = router
            .method(proto::SERVER_INFO, {
                let info = info.clone();
                let status_rows = status_rows.clone();
                move |_| {
                    let info = info.clone();
                    let status_rows = status_rows.clone();
                    async move {
                        let app = status_rows.as_ref().map(|rows| rows()).unwrap_or_default();
                        let report = StatusReport {
                            running: true,
                            version: Some(info.version),
                            pid: Some(info.pid),
                            managed_by: Some(info.managed_by),
                            app,
                        };
                        serde_json::to_value(report)
                            .map_err(|e| sapphire_ipc::RpcError::internal(e.to_string()))
                    }
                }
            })
            .method(sapphire_ipc::SHUTDOWN_METHOD, {
                let stop_tx = stop_tx.clone();
                move |_| {
                    let stop_tx = stop_tx.clone();
                    async move {
                        let _ = stop_tx.send(true);
                        serde_json::to_value(proto::Ack {})
                            .map_err(|e| sapphire_ipc::RpcError::internal(e.to_string()))
                    }
                }
            });
        if let Some(extend) = extend {
            router = extend(router);
        }
        let router = Arc::new(router);

        // Registered before the bind, so a SIGTERM arriving the instant the socket is up
        // cannot fall through to the default handler and kill the process. SIGINT is
        // listened to only through `ctrl_c` (which swallows the default handler on both
        // platforms): a Unix `SignalKind::interrupt()` stream would double-fire for ^C.
        #[cfg(unix)]
        let mut sigterm = Sigterm::new()?;
        #[cfg(not(unix))]
        let mut sigterm = Sigterm;

        #[cfg(unix)]
        let listener = sapphire_ipc::bind(&endpoint).await?;
        #[cfg(windows)]
        let mut listener = sapphire_ipc::bind(&endpoint)?;

        // The watcher, the bridge announcement loop and the dialer, if sync is on. Started
        // only once the socket is bound, so an early `?` above cannot leave tasks running for
        // a server that never served. All three are allowed to fail: only sync stops when
        // they do, and the app server keeps serving files (spec §10). They are aborted with
        // the server.
        let _sync_tasks = match &sync {
            Some(runtime) => {
                let driver = Arc::clone(runtime);
                let announcements = tokio::spawn(async move {
                    if let Err(err) = driver.run().await {
                        tracing::warn!("the bridge announcement loop ended: {err}");
                    }
                });
                let changing = Arc::clone(runtime);
                let watching = tokio::spawn(async move {
                    if let Err(err) = changing.watch_changes().await {
                        tracing::warn!("the file watcher ended: {err}");
                    }
                });
                // The dialer is what keeps a session open after the initial exchange, so a
                // commit reaches a peer without waiting for the next one (spec §4.2).
                let dialling = Arc::clone(runtime);
                let dialling = tokio::spawn(async move {
                    if let Err(err) = dialling.dial_loop().await {
                        tracing::warn!("the live dial loop ended: {err}");
                    }
                });
                Some((announcements, watching, dialling))
            }
            None => None,
        };

        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("interrupted; shutting down");
                    break;
                }
                _ = sigterm.recv() => {
                    tracing::info!("terminated; shutting down");
                    break;
                }
                accepted = listener.accept() => {
                    let conn = accepted?;
                    let router = Arc::clone(&router);
                    let app = ctx.app_name;
                    let info = info.clone();
                    connections.spawn(async move {
                        let _ = serve(conn, router, app, info).await;
                    });
                }
            }
        }

        if let Some(restoring) = restoring {
            restoring.abort();
        }
        if let Some((announcements, watching, dialling)) = _sync_tasks {
            announcements.abort();
            watching.abort();
            dialling.abort();
        }
        if let Some(runtime) = &sync {
            // Close every live sync session. A session's reader task holds the replica, and
            // the session itself sits in the runtime's table until it is cleared, so leaving
            // one open pins the runtime — and the redb replica stores in it — after the
            // server is gone (spec §2.2: cancellation is disconnection).
            runtime.drop_connections().await;
        }
        host.close();
        drop(listener); // removes the socket file on Unix

        // Reap the per-connection serve tasks. Each one pins the router, the router pins
        // the sync runtime, and the runtime's replica stores are redb databases that stay
        // locked until the last pin is dropped — a server that left them running would
        // lock its own successor out of its state (spec §2.2: cancellation is
        // disconnection). Connections get a grace period to wind down on their own — the
        // shutdown acknowledgement among them — then the rest are cancelled: a client
        // that keeps its connection open past shutdown is holding a server that is gone.
        let _ = tokio::time::timeout(SERVE_GRACE, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_ipc::{ClientInfo, Endpoint, ManagedBy};
    use sapphire_workspace::{AppContext, AppKind};
    use std::ffi::OsString;

    use crate::test_support;

    static CTX: AppContext = AppContext::new("sapphire-appservertest");

    /// The env vars `init_ctx` writes, in the order the guard restores them.
    const DIR_VARS: [&str; 3] = [
        "SAPPHIRE_APPSERVERTEST_CACHE_DIR",
        "SAPPHIRE_APPSERVERTEST_DATA_DIR",
        "SAPPHIRE_APPSERVERTEST_CONFIG_DIR",
    ];

    fn client_info() -> ClientInfo {
        ClientInfo {
            kind: "test".into(),
            version: "0.0.0".into(),
            api: sapphire_backend::protocol::API_VERSION,
            pid: std::process::id(),
        }
    }

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
            // the process environment in this test binary — the `host`, `handlers` and
            // `events` modules' tests share the same lock — and it is held until `drop`
            // has restored the old values.
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

    /// Everything one test needs, held for the test's whole body.
    ///
    /// `_tmp` is declared before `_env` so the scratch tree is deleted while the
    /// environment lock is still held: a sibling test that wakes on the lock must never
    /// observe the tree mid-deletion, nor env vars pointing at a deleted directory.
    struct Fixture {
        _tmp: tempfile::TempDir,
        _env: EnvGuard,
        endpoint: Endpoint,
    }

    /// A listening endpoint in a scratch directory, with the context's directories
    /// pointed at the same scratch tree.
    ///
    /// The caller must keep the fixture (and with it the environment lock) alive for the
    /// whole test: the static [`CTX`] is first-writer-wins, so while one test runs, every
    /// other test that would re-point the context's directories must wait.
    fn prepared() -> Fixture {
        let lock = test_support::lock();
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvGuard::set(lock, tmp.path());
        CTX.init(AppKind::Server);
        let endpoint = Endpoint::in_dir("sapphire-appservertest", tmp.path().to_path_buf());
        Fixture {
            _tmp: tmp,
            _env: env,
            endpoint,
        }
    }

    /// Wait until something is listening on `endpoint`.
    ///
    /// `tokio::spawn(server.run())` only schedules the server; without this wait a fast
    /// `connect_or_absent` probe can run before `run` has bound the socket, and with
    /// nothing listening that single unlucky probe is the whole test failing.
    async fn wait_until_listening(endpoint: &Endpoint) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !sapphire_ipc::probe(endpoint).await.unwrap() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the server never started listening"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A running server writes its log file: the framework's `logging` module
    /// routes the app's log to `<data dir>/logs/app.log`, and `run()` holds the
    /// guard for as long as it serves. The event is emitted by this test — the
    /// subscriber and the file layer are the process's, installed by `CTX.init`
    /// and `run` — so what lands in the file is exactly what a journald-less
    /// host would otherwise never see.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_server_writes_its_log_file() {
        let f = prepared();
        let endpoint = f.endpoint.clone();
        let server = AppServer::new(&CTX, "0.0.0").endpoint(endpoint.clone());
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;

        tracing::info!("the server is serving, and this line belongs in its log");

        let (client, _) = sapphire_ipc::connect_or_absent(&endpoint, CTX.app_name, client_info())
            .await
            .unwrap()
            .expect("the server is listening");
        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();
        // The guard flushes as `run` unwinds; awaiting it is what makes the
        // file complete before it is read.
        handle.await.unwrap().unwrap();

        let text = std::fs::read_to_string(CTX.log_dir().join("app.log")).unwrap();
        assert!(
            text.contains("the server is serving, and this line belongs in its log"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_reports_itself_as_service_managed() {
        let f = prepared();
        let endpoint = f.endpoint.clone();
        let server = AppServer::new(&CTX, "0.0.0").endpoint(endpoint.clone());
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;

        let (client, info) =
            sapphire_ipc::connect_or_absent(&endpoint, CTX.app_name, client_info())
                .await
                .unwrap()
                .expect("the server is listening");
        assert_eq!(info.managed_by, ManagedBy::Service);

        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_server_answers_server_info() {
        let f = prepared();
        let endpoint = f.endpoint.clone();

        let server = AppServer::new(&CTX, "1.2.3").endpoint(endpoint.clone());
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;

        let (client, info) = sapphire_ipc::connect_or_absent(
            &endpoint,
            CTX.app_name,
            ClientInfo {
                version: "1.2.3".into(),
                ..client_info()
            },
        )
        .await
        .unwrap()
        .expect("the server is listening");
        assert_eq!(info.version, "1.2.3");

        // `server.info` answers the whole typed report, not the bare handshake record:
        // the CLI reads one shape, and so would a GUI.
        let report: StatusReport = client
            .call(
                sapphire_backend::protocol::SERVER_INFO,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert!(report.running);
        assert_eq!(report.version.as_deref(), Some("1.2.3"));
        assert_eq!(report.pid, Some(std::process::id()));
        assert_eq!(report.managed_by, Some(ManagedBy::Service));
        assert!(report.app.is_empty(), "no status rows were configured");

        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_stops_the_server() {
        let f = prepared();
        let endpoint = f.endpoint.clone();

        let server = AppServer::new(&CTX, "0.0.0").endpoint(endpoint.clone());
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;

        let (client, _) = sapphire_ipc::connect_or_absent(&endpoint, CTX.app_name, client_info())
            .await
            .unwrap()
            .expect("the server is listening");
        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the server must stop")
            .unwrap()
            .unwrap();
        assert!(!sapphire_ipc::probe(&endpoint).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_application_can_add_its_own_methods() {
        let f = prepared();
        let endpoint = f.endpoint.clone();

        let server = AppServer::new(&CTX, "0.0.0")
            .endpoint(endpoint.clone())
            .extend(|r| {
                r.method("sapphire-appservertest.greet", |_| async move {
                    Ok(serde_json::json!("hello"))
                })
            });
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;

        let (client, _) = sapphire_ipc::connect_or_absent(&endpoint, CTX.app_name, client_info())
            .await
            .unwrap()
            .expect("the server is listening");
        let greeting: String = client
            .call("sapphire-appservertest.greet", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(greeting, "hello");

        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_init_creates_the_marker_and_tells_the_registry() {
        let f = prepared();
        let endpoint = f.endpoint.clone();
        let server = AppServer::new(&CTX, "0.0.0").endpoint(endpoint.clone());
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;
        let (client, _) = sapphire_ipc::connect_or_absent(&endpoint, CTX.app_name, client_info())
            .await
            .unwrap()
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let result: proto::WorkspaceInitResult = client
            .call(
                proto::WORKSPACE_INIT,
                proto::WorkspaceInitParams {
                    dir: dir.path().to_owned(),
                },
            )
            .await
            .unwrap();
        assert!(result.created);
        assert!(dir.path().join(format!(".{}", CTX.app_name)).is_dir());

        // Idempotent: the second init is a success that did not create.
        let again: proto::WorkspaceInitResult = client
            .call(
                proto::WORKSPACE_INIT,
                proto::WorkspaceInitParams {
                    dir: dir.path().to_owned(),
                },
            )
            .await
            .unwrap();
        assert!(!again.created);

        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_info_carries_the_app_rows() {
        let f = prepared();
        let endpoint = f.endpoint.clone();
        let server = AppServer::new(&CTX, "0.0.0")
            .endpoint(endpoint.clone())
            .status_rows(std::sync::Arc::new(|| {
                vec![StatusRow {
                    name: "sync".into(),
                    value: "enabled".into(),
                }]
            }));
        let handle = tokio::spawn(async move { server.run().await });
        wait_until_listening(&endpoint).await;
        let (client, _) = sapphire_ipc::connect_or_absent(&endpoint, CTX.app_name, client_info())
            .await
            .unwrap()
            .unwrap();
        let report: StatusReport = client
            .call(
                sapphire_backend::protocol::SERVER_INFO,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert!(report.running);
        assert_eq!(report.app.len(), 1);
        assert_eq!(report.app[0].name, "sync");

        let _: serde_json::Value = client
            .call(sapphire_ipc::SHUTDOWN_METHOD, serde_json::json!({}))
            .await
            .unwrap();
        handle.await.unwrap().unwrap();
    }
}
