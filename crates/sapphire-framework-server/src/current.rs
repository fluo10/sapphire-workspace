//! The one workspace a server serves, and what changing it does (#215).
//!
//! [`WorkspaceHost`] holds the open workspace; this decides which one it is. Selecting
//! another closes the current one — its index, its replica, its live sessions — and opens
//! the new one; sync follows the workspace that has it (decision 3 of the spec). Every
//! `workspace.*` and `sync.*` method acts through here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use grain_id::GrainId;
use sapphire_backend::protocol as proto;
use sapphire_backend::{WorkspaceEntry, WorkspaceRegistry};
use sapphire_ipc::{Router, RpcError};
use sapphire_workspace::{AppContext, Workspace};
use tokio::sync::Mutex as AsyncMutex;

use crate::error::{Error, Result};
use crate::handlers::rpc_error;
use crate::host::WorkspaceHost;
use crate::selection::{Selection, SelectionFile, slug};
use crate::sync::{SYNC_ID_FILE, SyncRuntime};

/// The server's current workspace.
pub struct Current {
    ctx: &'static AppContext,
    host: Arc<WorkspaceHost>,
    sync: Option<Arc<SyncRuntime>>,
    file: SelectionFile,
    /// One selection, and one sync switch, at a time.
    switching: AsyncMutex<()>,
}

impl std::fmt::Debug for Current {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Current")
            .field("root", &self.host.root())
            .finish()
    }
}

impl Current {
    /// The current workspace of `host`, synced by `sync` if there is one, remembered in
    /// `file`.
    pub fn new(
        ctx: &'static AppContext,
        host: Arc<WorkspaceHost>,
        sync: Option<Arc<SyncRuntime>>,
        file: SelectionFile,
    ) -> Current {
        Current {
            ctx,
            host,
            sync,
            file,
            switching: AsyncMutex::new(()),
        }
    }

    /// The host holding the open workspace.
    pub fn host(&self) -> &Arc<WorkspaceHost> {
        &self.host
    }

    /// The sync runtime, when this server syncs.
    pub fn sync(&self) -> Option<&Arc<SyncRuntime>> {
        self.sync.as_ref()
    }

    /// The current workspace's root.
    pub fn root(&self) -> Option<PathBuf> {
        self.host.root()
    }

    /// Make the workspace at `dir` current. Sync is on for it when it has a sync id.
    pub async fn select(&self, dir: &Path) -> Result<proto::CurrentWorkspace> {
        let root = resolve(dir)?;
        let sync = has_sync_id(self.ctx, &root);
        self.switch(&root, sync, false).await?;
        self.describe().await.ok_or(Error::NoWorkspace)
    }

    /// Create this app's workspace at `dir` — the marker and the marker registry's entry,
    /// idempotently — and make it current.
    pub async fn init(&self, dir: &Path) -> Result<proto::WorkspaceInitResult> {
        let result = init_workspace(self.ctx, dir)?;
        let sync = has_sync_id(self.ctx, &result.root);
        self.switch(&result.root, sync, false).await?;
        Ok(result)
    }

    /// Place the workgroup workspace `selector` at `dir`, make it current and sync it.
    pub async fn map(&self, selector: &str, dir: &Path) -> Result<GrainId> {
        let runtime = self.runtime()?;
        let root = runtime.place(selector, dir).await?;
        self.switch(&root, false, false).await?;
        self.enable_sync().await
    }

    /// Start syncing the current workspace, and remember that.
    pub async fn enable_sync(&self) -> Result<GrainId> {
        let runtime = self.runtime()?;
        let _one = self.switching.lock().await;
        let root = self.root().ok_or(Error::NoWorkspace)?;
        let id = runtime.enable(&root).await?;
        self.remember(&root, true);
        Ok(id)
    }

    /// Stop syncing the current workspace, and remember that.
    pub async fn disable_sync(&self) -> Result<()> {
        let runtime = self.runtime()?;
        let _one = self.switching.lock().await;
        let root = self.root().ok_or(Error::NoWorkspace)?;
        runtime.disable(&root).await?;
        self.remember(&root, false);
        Ok(())
    }

    /// The current workspace's sync state; not synced when there is none.
    pub async fn status(&self) -> proto::SyncStatusResult {
        match &self.sync {
            // An empty root names nothing, which the runtime reports as not synced, with
            // the bridge's view all the same.
            Some(runtime) => runtime
                .status(&self.root().unwrap_or_default())
                .await
                .into(),
            None => proto::SyncStatusResult::not_synced(),
        }
    }

    /// The current workspace as `workspace.current` reports it.
    pub async fn describe(&self) -> Option<proto::CurrentWorkspace> {
        let root = self.root()?;
        let marker = root.join(format!(".{}", self.ctx.app_name));
        // Read, never minted: describing must not create a sync id nobody asked for.
        let workspace_id = std::fs::read_to_string(marker.join(SYNC_ID_FILE))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let sync = match &self.sync {
            Some(runtime) => runtime.status(&root).await.into(),
            None => proto::SyncStatusResult::not_synced(),
        };
        Some(proto::CurrentWorkspace {
            reachable: marker.is_dir(),
            root,
            workspace_id,
            sync,
        })
    }

    /// Come back to the workspace `workspace.toml` names. A root that is gone stays
    /// selected, unopened, so `workspace.current` can say so.
    ///
    /// The open happens now, so the first client finds the workspace; sync, which talks to
    /// the bridge and may dial peers, starts on the returned task.
    pub(crate) async fn restore(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let selection = match self.file.load(self.ctx.app_name) {
            Ok(Some(selection)) => selection,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!("could not read the selected workspace: {err}");
                return None;
            }
        };
        {
            let _one = self.switching.lock().await;
            if let Err(err) = self.host.select(&selection.root, true).await {
                tracing::warn!(root = %selection.root.display(), "could not restore the workspace: {err}");
                return None;
            }
        }
        if !selection.sync {
            return None;
        }
        let runtime = self.sync.clone()?;
        let current = Arc::clone(self);
        Some(tokio::spawn(async move {
            // A switch that landed first wins: sync starts only for a workspace still
            // current.
            let _one = current.switching.lock().await;
            if current.root().as_deref() != Some(&selection.root) {
                return;
            }
            if let Err(err) = runtime.enable(&selection.root).await {
                tracing::warn!(root = %selection.root.display(), "could not restore sync: {err}");
            }
        }))
    }

    /// Make `root` current; sync it when `sync`. The previous workspace stops syncing.
    ///
    /// The open comes first: a workspace that does not open leaves the previous one current,
    /// syncing as before. `restoring` keeps a root that is gone selected, unopened.
    async fn switch(&self, root: &Path, sync: bool, restoring: bool) -> Result<()> {
        let _one = self.switching.lock().await;
        let previous = self.root();
        self.host.select(root, restoring).await?;
        if let Some(runtime) = &self.sync {
            if let Some(previous) = previous.filter(|p| p != root)
                && let Err(err) = runtime.disable(&previous).await
            {
                tracing::warn!(root = %previous.display(), "stopping sync of the previous workspace: {err}");
            }
            if sync && let Err(err) = runtime.enable(root).await {
                // Selected all the same: the workspace works without sync, and the status
                // carries the failure.
                tracing::warn!(root = %root.display(), "could not start sync: {err}");
            }
        }
        self.remember(root, sync);
        Ok(())
    }

    fn remember(&self, root: &Path, sync: bool) {
        let selection = Selection {
            root: root.to_owned(),
            sync,
        };
        if let Err(err) = self.file.save(&selection) {
            tracing::warn!(root = %root.display(), "could not record the selected workspace: {err}");
        }
    }

    fn runtime(&self) -> Result<&Arc<SyncRuntime>> {
        self.sync
            .as_ref()
            .ok_or_else(|| Error::Config("this server does not sync".to_owned()))
    }
}

/// Whether `root` has synced before: it carries a sync id.
fn has_sync_id(ctx: &AppContext, root: &Path) -> bool {
    root.join(format!(".{}", ctx.app_name))
        .join(SYNC_ID_FILE)
        .is_file()
}

/// `dir` as a canonical root. A relative `dir` is resolved against the server's cwd.
fn resolve(dir: &Path) -> Result<PathBuf> {
    std::env::current_dir()
        .map_err(Error::Io)?
        .join(dir)
        .canonicalize()
        .map_err(Error::Io)
}

/// Add `workspace.init`, `workspace.select` and `workspace.current` to `router`.
pub(crate) fn current_methods(current: Arc<Current>, router: Router) -> Router {
    let init = Arc::clone(&current);
    let select = Arc::clone(&current);
    router
        .method(proto::WORKSPACE_INIT, move |req| {
            let current = Arc::clone(&init);
            async move {
                let p: proto::WorkspaceInitParams = serde_json::from_value(req.params)
                    .map_err(|e| RpcError::invalid_params(format!("bad parameters: {e}")))?;
                let result = current.init(&p.dir).await.map_err(|e| rpc_error(&e))?;
                serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
            }
        })
        .method(proto::WORKSPACE_SELECT, move |req| {
            let current = Arc::clone(&select);
            async move {
                let p: proto::WorkspaceSelectParams = serde_json::from_value(req.params)
                    .map_err(|e| RpcError::invalid_params(format!("bad parameters: {e}")))?;
                let result = current.select(&p.dir).await.map_err(|e| rpc_error(&e))?;
                serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
            }
        })
        .method(proto::WORKSPACE_CURRENT, move |_| {
            let current = Arc::clone(&current);
            async move {
                let workspace = current.describe().await;
                serde_json::to_value(proto::WorkspaceCurrentResult { workspace })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }
        })
}

/// Create the workspace home at `dir`: the marker directory and the marker registry's
/// entry, idempotently. The sync id is `sync.enable` / `sync.map`'s to mint, not this one's.
///
/// A relative `dir` is resolved against the server's cwd. The registry entry's id is the
/// root's directory name, slugified; `created` is `false` when the marker was already there,
/// and an entry already there is kept as it is.
fn init_workspace(ctx: &'static AppContext, dir: &Path) -> Result<proto::WorkspaceInitResult> {
    let root = resolve(dir)?;
    let marker = root.join(format!(".{}", ctx.app_name));
    let created = !marker.is_dir();
    if created {
        std::fs::create_dir(&marker).map_err(Error::Io)?;
    }

    // Reads the marker's `config.toml`, keyed the way the apps' CLIs key their
    // `--workspace` selectors. The registry lives in the marker, so it travels with the
    // workspace when it syncs.
    let workspace = Workspace::from_root(ctx, &root)?;
    let id = slug(&root);
    let config_path = workspace.config_path();
    let registry = read_registry(&config_path)?;
    if registry.get(&id).is_none() {
        let mut registry = registry;
        registry.insert(id.clone(), WorkspaceEntry::local(&root));
        write_registry(&config_path, &registry)?;
    }

    Ok(proto::WorkspaceInitResult {
        root,
        workspace_id: id,
        created,
    })
}

/// The registry as the marker's `config.toml` holds it, or an empty one.
///
/// A file another application wrote without a `[workspace]` table is an empty registry,
/// not an error: the marker's config is the app's own file, and a workspace created
/// before this table existed is a workspace with no entries.
fn read_registry(path: &Path) -> Result<WorkspaceRegistry> {
    #[derive(serde::Deserialize, Default)]
    struct Config {
        #[serde(default)]
        workspace: WorkspaceRegistry,
    }
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let config: Config = toml::from_str(&text).map_err(|e| Error::SyncId(e.to_string()))?;
            Ok(config.workspace)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WorkspaceRegistry::default()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Write the registry back into the marker's `config.toml`, keeping the rest of the file.
///
/// The marker's config is the app's own document, and a rewrite that dropped the rest of
/// it would eat an application's settings.
fn write_registry(path: &Path, registry: &WorkspaceRegistry) -> Result<()> {
    #[derive(serde::Deserialize, serde::Serialize, Default)]
    struct Config {
        #[serde(default, skip_serializing_if = "WorkspaceRegistry::is_empty")]
        workspace: WorkspaceRegistry,
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut config: Config = toml::from_str(&text).unwrap_or_default();
    config.workspace = registry.clone();
    let out = toml::to_string_pretty(&config).map_err(|e| Error::SyncId(e.to_string()))?;
    std::fs::write(path, out).map_err(Error::Io)
}
