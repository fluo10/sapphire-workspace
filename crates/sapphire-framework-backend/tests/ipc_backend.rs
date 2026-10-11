//! `IpcBackend` must behave like `LocalBackend`, because the UI cannot tell them apart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sapphire_backend::{IpcBackend, SearchMode, WorkspaceBackend};
use sapphire_ipc::Endpoint;

/// Where `cargo test` put the test server binary.
///
/// Cargo only defines `CARGO_BIN_EXE_*` for binaries of the package being tested, and
/// `server-test-app` belongs to the dev-dependency `sapphire-framework-server`, so the
/// variable is not set here. The binary is still built — the dev-dependency triggers it —
/// next to this test harness: the harness runs from `{target}/{profile}/deps/`, so the
/// profile directory one level up holds `server-test-app`.
fn server_test_app_exe() -> PathBuf {
    if let Some(exe) = std::env::var_os("CARGO_BIN_EXE_server-test-app") {
        return PathBuf::from(exe);
    }
    let deps_dir = std::env::current_exe()
        .expect("the test binary's path")
        .parent()
        .expect("deps directory")
        .to_path_buf();
    deps_dir
        .parent()
        .expect("profile directory")
        .join("server-test-app")
}

/// Start the test application server from `sapphire-framework-server`, and return the
/// endpoint plus a workspace root inside it.
async fn fixture(tmp: &Path) -> (Endpoint, PathBuf, std::process::Child) {
    let runtime = tmp.join("run");
    let state = tmp.join("state");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    let root = tmp.join("ws");
    std::fs::create_dir_all(root.join(".sapphire-servertest")).unwrap();
    let root = root.canonicalize().unwrap();

    let endpoint = Endpoint::in_dir("sapphire-servertest", runtime.clone());
    // One server process for the whole test, the way `serve` or the service manager
    // would run one. Nothing here starts a server on demand any more. The child is
    // detached on purpose — it must outlive the test that started it, and the temp
    // directory's removal ends it.
    let child = std::process::Command::new(server_test_app_exe())
        .args([runtime.display().to_string(), state.display().to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the test server");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !sapphire_ipc::probe(&endpoint).await.unwrap_or(false) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server never started listening"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    (endpoint, root, child)
}

/// Make `ws` the server's workspace, as a CLI's `workspace select` does.
async fn select(backend: &IpcBackend, ws: &Path) {
    let _: sapphire_backend::protocol::CurrentWorkspace = backend
        .client()
        .call(
            sapphire_backend::protocol::WORKSPACE_SELECT,
            sapphire_backend::protocol::WorkspaceSelectParams { dir: ws.to_owned() },
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ipc_backend_reads_back_what_it_wrote() {
    let tmp = tempfile::tempdir().unwrap();
    let (endpoint, ws, mut server) = fixture(tmp.path()).await;

    let backend = IpcBackend::connect(
        &endpoint,
        "sapphire-servertest",
        "test",
        env!("CARGO_PKG_VERSION"),
    )
    .await
    .unwrap();
    select(&backend, &ws).await;
    assert_eq!(backend.current().await.unwrap().unwrap().root, ws);

    backend
        .write_file(Path::new("a.md"), "# hello")
        .await
        .unwrap();
    assert_eq!(
        backend.read_file(Path::new("a.md")).await.unwrap(),
        "# hello"
    );

    let hits = backend.search("hello", 10, SearchMode::Fts).await.unwrap();
    assert!(hits.iter().any(|h| h.path.ends_with("a.md")), "{hits:?}");

    let listing = backend.list_dir(Path::new(".")).await.unwrap();
    assert!(
        listing.iter().any(|(p, _)| p.ends_with("a.md")),
        "{listing:?}"
    );

    backend.delete_file(Path::new("a.md")).await.unwrap();
    assert!(backend.read_file(Path::new("a.md")).await.is_err());

    let _ = server.kill();
}

#[tokio::test(flavor = "multi_thread")]
async fn events_reach_a_subscriber_of_an_ipc_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let (endpoint, ws, mut server) = fixture(tmp.path()).await;

    let backend = Arc::new(
        IpcBackend::connect(
            &endpoint,
            "sapphire-servertest",
            "test",
            env!("CARGO_PKG_VERSION"),
        )
        .await
        .unwrap(),
    );
    select(&backend, &ws).await;

    // `subscribe` is synchronous, so asking the server to start sending is a separate call.
    backend.start_events().await.unwrap();
    let mut events = backend.subscribe();
    backend.write_file(Path::new("a.md"), "x").await.unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
        .await
        .expect("an event within ten seconds")
        .unwrap();
    assert!(
        matches!(event, sapphire_backend::BackendEvent::FileChanged { .. }),
        "{event:?}"
    );

    let _ = server.kill();
}
