use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(feature = "sqlite-store")]
use sapphire_retrieve::db::SCHEMA_VERSION;
#[cfg(feature = "lancedb-store")]
use sapphire_retrieve::open_lancedb;
use sapphire_retrieve::{
    Document, Embedder, FileSearchResult, FtsQuery, HybridQuery, RetrieveStore, VectorQuery,
};
#[cfg(feature = "sqlite-store")]
use sapphire_retrieve::{open_sqlite_fts, open_sqlite_vec};
use tokio::sync::OnceCell;

use crate::{
    config::{HybridConfig, RetrieveConfig, VectorDb},
    error::{Error, Result},
    indexer::{path_to_doc_id, sync_workspace, sync_workspace_incremental},
    workspace::Workspace,
};

use sapphire_retrieve::build_embedder;

/// Controls which retrieval strategy [`WorkspaceState::retrieve_files`] uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchMode {
    /// Full-text search only (BM25 / trigram).
    Fts,
    /// Semantic (vector) search only.  Falls back to FTS if no embedder is
    /// configured.
    Semantic,
    /// Combine FTS and semantic results via Reciprocal Rank Fusion (default).
    #[default]
    Hybrid,
}

/// Parameters for [`WorkspaceState::retrieve_files`].
pub struct RetrieveParams<'a> {
    /// The search query string.
    pub query: &'a str,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Retrieval strategy (default: [`SearchMode::Hybrid`]).
    pub mode: SearchMode,
    /// Optional folder prefix filter.  Only results whose path starts with
    /// this prefix are returned.  Should be an absolute path.
    pub folder: Option<&'a Path>,
}

/// An open workspace paired with its lazily-initialised search infrastructure.
pub struct WorkspaceState {
    pub workspace: Workspace,
    retrieve_db: Mutex<Arc<dyn RetrieveStore + Send + Sync>>,
    embedder: OnceCell<Option<Box<dyn Embedder + Send + Sync>>>,
    sync_backend: Option<Box<dyn sapphire_sync::SyncBackend + Send + Sync>>,
}

/// Database statistics returned by [`WorkspaceState::db_info`].
pub struct DbInfo {
    pub db_path: PathBuf,
    pub schema_version: i32,
    pub document_count: u64,
    pub embedding_dim: u32,
    pub vector_count: u64,
    pub pending_count: u64,
}

// ── path resolution helpers ──────────────────────────────────────────────────

/// Result of resolving a caller-supplied path against the workspace root.
enum ResolvedPath {
    /// The path is inside the workspace.
    Internal(PathBuf),
    /// The path is outside the workspace.
    External(PathBuf),
}

impl ResolvedPath {
    fn as_path(&self) -> &Path {
        match self {
            Self::Internal(p) | Self::External(p) => p,
        }
    }

    fn is_internal(&self) -> bool {
        matches!(self, Self::Internal(_))
    }
}

/// Canonicalize `path`, falling back to canonicalizing the nearest existing
/// ancestor and appending the remaining components.  This is necessary for
/// paths that do not exist yet (e.g. a new file being created).
fn canonicalize_or_parent(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(p) = path.canonicalize() {
        return Ok(p);
    }
    // Walk up until we find an existing ancestor.
    let mut suffix = PathBuf::new();
    let mut current = path;
    loop {
        if let Some(parent) = current.parent() {
            let name = current.file_name().unwrap_or(current.as_os_str());
            suffix = Path::new(name).join(&suffix);
            match parent.canonicalize() {
                Ok(canon) => return Ok(canon.join(suffix)),
                Err(_) => current = parent,
            }
        } else {
            // No existing ancestor at all — return the path as-is.
            return Ok(path.to_owned());
        }
    }
}

impl WorkspaceState {
    /// Open (or create) the retrieve DB for `workspace`.
    ///
    /// When the `git-sync` feature is enabled, automatically attaches a
    /// [`sapphire_sync::GitSync`] backend if the workspace root is inside a
    /// git repository.  Silently falls back to no backend if git is not found.
    pub fn open(workspace: Workspace) -> Result<Self> {
        let backend = Self::open_initial_backend(&workspace);
        let mut state = Self {
            retrieve_db: Mutex::new(backend),
            workspace,
            embedder: OnceCell::new(),
            sync_backend: None,
        };
        #[cfg(feature = "git-sync")]
        if let Ok(git) = sapphire_sync::GitSync::open(&state.workspace.root) {
            state.set_sync_backend(Box::new(git));
        }
        Ok(state)
    }

    /// Delete and recreate the retrieve DB from scratch.
    pub fn rebuild(workspace: Workspace) -> Result<Self> {
        #[cfg(feature = "sqlite-store")]
        sapphire_retrieve::sqlite_store::wipe_db_files(&workspace.retrieve_db_path());
        #[cfg(feature = "lancedb-store")]
        {
            use sapphire_retrieve::lancedb_store;
            let _ = std::fs::remove_dir_all(lancedb_store::data_dir(&workspace.cache_dir()));
        }
        let backend = Self::open_initial_backend(&workspace);
        Ok(Self {
            retrieve_db: Mutex::new(backend),
            workspace,
            embedder: OnceCell::new(),
            sync_backend: None,
        })
    }

    /// Open workspace and configure the sync backend from [`SyncConfig`].
    ///
    /// - `SyncBackendKind::Auto` (default) — same as [`open`](Self::open):
    ///   attach git if a repository is found, silently no-op otherwise.
    /// - `SyncBackendKind::Git` — attach git with the configured remote;
    ///   returns an error if no repository is found.
    /// - `SyncBackendKind::None` — disable sync even inside a git repository.
    #[cfg(feature = "git-sync")]
    pub fn open_configured(workspace: Workspace, sync: &crate::config::SyncConfig) -> Result<Self> {
        use crate::config::SyncBackendKind;
        let mut state = Self::open(workspace)?;
        match sync.workspace.backend {
            SyncBackendKind::Auto => {
                // Re-create the backend so we can apply the device_id commit message.
                if let Ok(git) = sapphire_sync::GitSync::open(&state.workspace.root) {
                    state.set_sync_backend(Box::new(Self::apply_device_id(git, sync)));
                }
            }
            SyncBackendKind::Git => {
                // Explicit git: use the configured remote and fail hard if
                // no repository is found.
                let git =
                    sapphire_sync::GitSync::with_remote(&state.workspace.root, sync.remote())?;
                state.set_sync_backend(Box::new(Self::apply_device_id(git, sync)));
            }
            SyncBackendKind::None => {
                // Explicitly disabled: remove whatever `open` may have set.
                state.sync_backend = None;
            }
        }
        Ok(state)
    }

    /// Apply `device_id` from the sync config as the git commit message.
    #[cfg(feature = "git-sync")]
    fn apply_device_id(
        git: sapphire_sync::GitSync,
        sync: &crate::config::SyncConfig,
    ) -> sapphire_sync::GitSync {
        match sync.user.device_id {
            Some(id) => git.with_commit_message(format!("auto: sync [{id}]")),
            None => git,
        }
    }

    /// Open workspace and configure the sync backend from [`SyncConfig`].
    /// (no-op version when the `git-sync` feature is not compiled in)
    #[cfg(not(feature = "git-sync"))]
    pub fn open_configured(
        workspace: Workspace,
        _sync: &crate::config::SyncConfig,
    ) -> Result<Self> {
        Self::open(workspace)
    }

    /// Borrow the sync backend, if one is configured.
    pub fn sync_backend(&self) -> Option<&dyn sapphire_sync::SyncBackend> {
        self.sync_backend
            .as_ref()
            .map(|b| b.as_ref() as &dyn sapphire_sync::SyncBackend)
    }

    /// Attach a sync backend (e.g. `GitSync`).  Called once after construction.
    pub fn set_sync_backend(&mut self, backend: Box<dyn sapphire_sync::SyncBackend + Send + Sync>) {
        self.sync_backend = Some(backend);
    }

    /// Clone the active retrieve backend as an `Arc<dyn RetrieveStore>`.
    ///
    /// The lock is released immediately after cloning the `Arc`, so long-running
    /// operations do not block other threads from checking the backend state.
    pub fn retrieve_db(&self) -> Arc<dyn RetrieveStore + Send + Sync> {
        Arc::clone(&*self.retrieve_db.lock().unwrap())
    }

    pub fn embedder(&self) -> Option<&dyn Embedder> {
        Some(self.embedder.get()?.as_ref()?.as_ref())
    }

    // ── single-file update API ────────────────────────────────────────────────

    /// Update the retrieve index for a single file and stage it via the sync
    /// backend (if configured).
    ///
    /// Reads the file from disk, upserts it into the retrieve DB, and calls
    /// `sync_backend.add_file` when a backend is attached.
    pub fn on_file_updated(&self, path: &Path) -> Result<()> {
        let resolved = self.resolve_path(path)?;
        if !resolved.is_internal() {
            return Ok(());
        }
        let abs = resolved.as_path();
        let path_str = abs.to_string_lossy().into_owned();

        let mtime = abs
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            })
            .unwrap_or(0);

        let body = std::fs::read_to_string(abs)?;
        let doc_id = path_to_doc_id(abs);

        let db = self.retrieve_db();
        db.upsert_file(&path_str, mtime)?;
        db.upsert_document(&Document {
            id: doc_id,
            body,
            path: path_str,
            chunks: None,
        })?;
        db.rebuild_fts()?;

        if let Some(sync) = &self.sync_backend {
            sync.add_file(abs)?;
        }

        Ok(())
    }

    /// Remove a file from the retrieve index and unstage it via the sync
    /// backend (if configured).
    ///
    /// External paths are silently ignored when
    /// [`allow_external_paths`](crate::AppContext::allow_external_paths) is
    /// enabled; otherwise returns [`Error::PathEscapesWorkspace`].
    pub fn on_file_deleted(&self, path: &Path) -> Result<()> {
        let resolved = self.resolve_path(path)?;
        if !resolved.is_internal() {
            return Ok(());
        }
        let abs = resolved.as_path();
        let path_str = abs.to_string_lossy().into_owned();
        let doc_id = path_to_doc_id(abs);

        let db = self.retrieve_db();
        db.remove_document(doc_id)?;
        db.remove_file(&path_str)?;
        db.rebuild_fts()?;

        if let Some(sync) = &self.sync_backend {
            sync.remove_file(abs)?;
        }

        Ok(())
    }

    // ── path resolution ─────────��──────────────────────��───────────────────────

    /// Resolve `path` to an absolute path and classify it as internal or
    /// external to the workspace.
    ///
    /// Returns [`Error::PathEscapesWorkspace`] when the resolved path is
    /// outside the workspace **and**
    /// [`AppContext::allows_external_paths`](crate::AppContext::allows_external_paths)
    /// is `false`.
    fn resolve_path(&self, path: &Path) -> Result<ResolvedPath> {
        let joined = if path.is_absolute() {
            path.to_owned()
        } else {
            self.workspace.root.join(path)
        };
        let abs = canonicalize_or_parent(&joined)?;

        if abs.starts_with(&self.workspace.root) {
            Ok(ResolvedPath::Internal(abs))
        } else if self.workspace.ctx.allows_external_paths() {
            Ok(ResolvedPath::External(abs))
        } else {
            Err(Error::PathEscapesWorkspace {
                path: path.to_owned(),
                root: self.workspace.root.clone(),
            })
        }
    }

    // ── file operations ─────────────────────────────────────────────────────
    //
    // These methods accept either relative or absolute paths.  Relative paths
    // are resolved against the workspace root.  For paths inside the
    // workspace, the retrieve index and sync backend are updated
    // automatically.  External paths (when permitted) use plain `std::fs`.

    /// Read a text file and return its contents as a `String`.
    pub fn read_file(&self, path: &Path) -> Result<String> {
        let resolved = self.resolve_path(path)?;
        Ok(std::fs::read_to_string(resolved.as_path())?)
    }

    /// Read a line range from a text file.
    ///
    /// `start_line` and `end_line` are **1-indexed** and **inclusive**.
    /// `end_line: None` reads to the end of the file.
    /// Lines beyond the end of the file are silently clamped.
    pub fn read_file_range(
        &self,
        path: &Path,
        start_line: usize,
        end_line: Option<usize>,
    ) -> Result<String> {
        let resolved = self.resolve_path(path)?;
        let content = std::fs::read_to_string(resolved.as_path())?;
        let start = start_line.saturating_sub(1); // convert to 0-indexed
        let lines: Vec<&str> = content.lines().collect();
        let end = end_line.map(|e| e.min(lines.len())).unwrap_or(lines.len());
        let slice = if start >= lines.len() {
            &[] as &[&str]
        } else {
            &lines[start..end]
        };
        Ok(slice.join("\n"))
    }

    /// List the direct children of a directory.
    ///
    /// For internal (workspace) directories, returns pairs of
    /// `(workspace-relative path, is_dir)`.  For external directories,
    /// returns `(absolute path, is_dir)`.  Sorted alphabetically by path.
    pub fn list_dir(&self, path: &Path) -> Result<Vec<(PathBuf, bool)>> {
        let resolved = self.resolve_path(path)?;
        let abs = resolved.as_path();
        let is_internal = resolved.is_internal();
        let mut entries: Vec<(PathBuf, bool)> = std::fs::read_dir(abs)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let entry_path = if is_internal {
                    e.path().strip_prefix(&self.workspace.root).ok()?.to_owned()
                } else {
                    e.path()
                };
                Some((entry_path, is_dir))
            })
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Write `content` to a file.
    ///
    /// Creates any missing parent directories automatically.
    /// Overwrites the file if it already exists.
    /// For internal files, updates the retrieve index and sync backend.
    pub fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        let resolved = self.resolve_path(path)?;
        let abs = resolved.as_path();
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(abs, content)?;
        if resolved.is_internal() {
            self.on_file_updated(abs)?;
        }
        Ok(())
    }

    /// Append `content` to a file.
    ///
    /// Creates the file (and any missing parent directories) if it does not
    /// exist yet.
    /// For internal files, updates the retrieve index and sync backend.
    pub fn append_file(&self, path: &Path, content: &str) -> Result<()> {
        use std::io::Write as _;
        let resolved = self.resolve_path(path)?;
        let abs = resolved.as_path();
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(abs)?;
        file.write_all(content.as_bytes())?;
        drop(file);
        if resolved.is_internal() {
            self.on_file_updated(abs)?;
        }
        Ok(())
    }

    /// Delete a file from disk.
    ///
    /// For internal files, also removes it from the retrieve index and sync
    /// backend.
    pub fn delete_file(&self, path: &Path) -> Result<()> {
        let resolved = self.resolve_path(path)?;
        let abs = resolved.as_path();
        std::fs::remove_file(abs)?;
        if resolved.is_internal() {
            self.on_file_deleted(abs)?;
        }
        Ok(())
    }

    // ── vector backend ────────────────────────────────────────────────────────

    /// Initialise the vector backend (sync). Idempotent.
    pub fn load_retrieve_backend(&self, retrieve: &RetrieveConfig) -> Result<()> {
        let Some((vector_db, dim)) = Self::extract_vector_config(retrieve) else {
            return Ok(());
        };
        if let Some(backend) = self.make_vector_backend(vector_db, dim)? {
            *self.retrieve_db.lock().unwrap() = backend;
        }
        Ok(())
    }

    /// Async version of [`load_retrieve_backend`](Self::load_retrieve_backend).
    pub async fn load_retrieve_backend_async(&self, retrieve: &RetrieveConfig) -> Result<()> {
        self.load_retrieve_backend(retrieve)
    }

    // ── embedder ──────────────────────────────────────────────────────────────

    /// Initialise the embedder (sync). Idempotent.
    pub fn load_embedder(&self, retrieve: &RetrieveConfig) -> Result<()> {
        if self.embedder.initialized() {
            return Ok(());
        }
        let embedder = retrieve
            .embedding
            .as_ref()
            .filter(|c| c.enabled)
            .map(|c| {
                let mut cfg = c.to_embedder_config();
                cfg.cache_dir = Some(self.workspace.ctx.model_cache_dir());
                build_embedder(&cfg)
            })
            .transpose()?;
        let _ = self.embedder.set(embedder);
        Ok(())
    }

    /// Async version of [`load_embedder`](Self::load_embedder).
    pub async fn load_embedder_async(&self, retrieve: &RetrieveConfig) -> Result<()> {
        let model_cache_dir = self.workspace.ctx.model_cache_dir();
        self.embedder
            .get_or_try_init(|| async {
                retrieve
                    .embedding
                    .as_ref()
                    .filter(|c| c.enabled)
                    .map(|c| {
                        let mut cfg = c.to_embedder_config();
                        cfg.cache_dir = Some(model_cache_dir.clone());
                        build_embedder(&cfg)
                    })
                    .transpose()
            })
            .await?;
        Ok(())
    }

    // ── bulk sync ─────────────────────────────────────────────────────────────

    /// Scan the workspace and incrementally sync all files into the retrieve DB.
    pub fn sync(&self) -> Result<(usize, usize)> {
        sync_workspace(&self.workspace, self.retrieve_db())
    }

    /// Run a mtime-based incremental retrieve cache refresh.
    ///
    /// Only re-indexes files whose mtime has changed since the last run.
    /// Does **not** perform any git sync.
    ///
    /// Returns `(upserted, removed)`.
    pub fn sync_retrieve(&self) -> Result<(usize, usize)> {
        sync_workspace_incremental(&self.workspace, self.retrieve_db())
    }

    /// Run a full git sync cycle (commit → pull → push), if a sync backend is
    /// configured.  Does **not** update the retrieve cache.
    pub fn sync_git(&self) -> Result<()> {
        if let Some(backend) = &self.sync_backend {
            backend.sync()?;
        }
        Ok(())
    }

    /// Run the periodic sync cycle: git sync (if configured) followed by an
    /// mtime-based incremental cache update.
    ///
    /// Convenience wrapper that calls [`sync_git`](Self::sync_git) then
    /// [`sync_retrieve`](Self::sync_retrieve).
    ///
    /// Returns `(upserted, removed)`.
    pub fn periodic_sync(&self) -> Result<(usize, usize)> {
        self.sync_git()?;
        self.sync_retrieve()
    }

    /// Sync and, when embedding is configured, embed pending chunks.
    ///
    /// Returns `(upserted, removed, embedded)`.
    pub async fn sync_and_embed(&self, retrieve: &RetrieveConfig) -> Result<(usize, usize, usize)> {
        let (upserted, removed) = sync_workspace(&self.workspace, self.retrieve_db())?;

        let Some(embed_cfg) = retrieve.embedding.as_ref() else {
            return Ok((upserted, removed, 0));
        };
        if !embed_cfg.enabled {
            return Ok((upserted, removed, 0));
        }

        self.load_retrieve_backend_async(retrieve).await?;
        self.load_embedder_async(retrieve).await?;

        let Some(embedder) = self.embedder() else {
            return Ok((upserted, removed, 0));
        };

        let embedded = self.retrieve_db().embed_pending(embedder, &|_, _| {})?;
        Ok((upserted, removed, embedded))
    }

    /// Embed all pending chunks (sync). Loads backend and embedder if needed.
    pub fn embed_pending(
        &self,
        retrieve: &RetrieveConfig,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        let Some(embed_cfg) = retrieve.embedding.as_ref() else {
            return Ok(0);
        };
        if !embed_cfg.enabled {
            return Ok(0);
        }
        self.load_retrieve_backend(retrieve)?;
        self.load_embedder(retrieve)?;
        let Some(embedder) = self.embedder() else {
            return Ok(0);
        };
        Ok(self.retrieve_db().embed_pending(embedder, &on_progress)?)
    }

    // ── info ──────────────────────────────────────────────────────────────────

    pub fn db_info(&self) -> Result<DbInfo> {
        let db_path = self.workspace.retrieve_db_path();
        let db = self.retrieve_db();
        let document_count = db.document_count().unwrap_or(0);
        let vec_info = db.vec_info().unwrap_or(sapphire_retrieve::VecInfo {
            embedding_dim: 0,
            vector_count: 0,
            pending_count: 0,
        });
        Ok(DbInfo {
            db_path,
            #[cfg(feature = "sqlite-store")]
            schema_version: SCHEMA_VERSION,
            #[cfg(not(feature = "sqlite-store"))]
            schema_version: 0,
            document_count,
            embedding_dim: vec_info.embedding_dim,
            vector_count: vec_info.vector_count,
            pending_count: vec_info.pending_count,
        })
    }

    // ── retrieve (unified search) ────────────────────────────────────────────

    /// Retrieve files matching `query` using the specified search mode.
    ///
    /// - **Fts**: full-text search only.
    /// - **Semantic**: vector search only (falls back to FTS when no embedder
    ///   is loaded).
    /// - **Hybrid** (default): runs both FTS and semantic search, then merges
    ///   results via Reciprocal Rank Fusion (RRF).
    ///
    /// When `params.folder` is set, results are post-filtered to paths that
    /// start with that prefix.
    pub fn retrieve_files(
        &self,
        params: &RetrieveParams<'_>,
        hybrid_config: &HybridConfig,
    ) -> Result<Vec<FileSearchResult>> {
        // Fall back to FTS when the embedder is not available.
        let effective_mode = match params.mode {
            SearchMode::Semantic if self.embedder().is_none() => SearchMode::Fts,
            other => other,
        };

        let results = match effective_mode {
            SearchMode::Fts => {
                let mut q = FtsQuery::new(params.query).limit(params.limit);
                if let Some(f) = params.folder {
                    q = q.path_prefix(f);
                }
                self.retrieve_db().search_fts(&q)?
            }
            SearchMode::Semantic => {
                let embedder = self.embedder().expect("caller verified embedder exists");
                let mut vq = VectorQuery::new(params.query, embedder).limit(params.limit);
                if let Some(f) = params.folder {
                    vq = vq.path_prefix(f);
                }
                self.retrieve_db().search_similar(&vq)?
            }
            SearchMode::Hybrid => {
                let mut hq = HybridQuery::new(params.query)
                    .limit(params.limit)
                    .rrf_k(hybrid_config.rrf_k as f64)
                    .weight_fts(hybrid_config.fts_weight)
                    .weight_sem(1.0 - hybrid_config.fts_weight);
                if let Some(e) = self.embedder() {
                    hq = hq.embedder(e);
                }
                if let Some(f) = params.folder {
                    hq = hq.path_prefix(f);
                }
                self.retrieve_db().search_hybrid(&hq)?
            }
        };

        Ok(results)
    }

    // ── private helpers ───────────────────────────────────────────────────────

    /// Create the initial (non-vector) backend appropriate for the compiled features.
    fn open_initial_backend(workspace: &Workspace) -> Arc<dyn RetrieveStore + Send + Sync> {
        #[cfg(feature = "sqlite-store")]
        {
            open_sqlite_fts(&workspace.retrieve_db_path())
        }
        #[cfg(not(feature = "sqlite-store"))]
        {
            let _ = workspace;
            sapphire_retrieve::open_in_memory()
        }
    }

    /// Extract `(vector_db, embedding_dim)` from config if vector search is enabled.
    fn extract_vector_config(retrieve: &RetrieveConfig) -> Option<(VectorDb, u32)> {
        let embed_cfg = retrieve.embedding.as_ref()?;
        if !embed_cfg.enabled {
            return None;
        }
        let dim = embed_cfg.dimension?;
        Some((retrieve.db, dim))
    }

    /// Construct a fully-initialised vector backend for the given `vector_db` kind.
    ///
    /// Returns `None` when `vector_db` is `VectorDb::None` (no vector search).
    fn make_vector_backend(
        &self,
        vector_db: VectorDb,
        dim: u32,
    ) -> Result<Option<Arc<dyn RetrieveStore + Send + Sync>>> {
        match vector_db {
            VectorDb::None => Ok(None),
            #[cfg(feature = "sqlite-store")]
            VectorDb::SqliteVec => Ok(Some(open_sqlite_vec(
                &self.workspace.retrieve_db_path(),
                dim,
            )?)),
            #[cfg(not(feature = "sqlite-store"))]
            VectorDb::SqliteVec => Err(crate::error::Error::SqliteStoreNotEnabled),
            #[cfg(feature = "lancedb-store")]
            VectorDb::LanceDb => {
                use sapphire_retrieve::lancedb_store;
                let lancedb_dir = lancedb_store::data_dir(&self.workspace.cache_dir());
                Ok(Some(open_lancedb(&lancedb_dir, dim)?))
            }
            #[cfg(not(feature = "lancedb-store"))]
            VectorDb::LanceDb => Err(crate::error::Error::LanceDbNotEnabled),
        }
    }
}
