use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sapphire_retrieve::{
    Embedder, FileSearchResult, FtsQuery, HybridQuery, RetrieveStore, VectorQuery,
};
#[cfg(feature = "redb-store")]
use sapphire_retrieve::{open_redb, open_redb_vec};
use sapphire_track::TrackStore;
use tokio::sync::OnceCell;

use crate::{
    config::{HybridConfig, RetrieveConfig, VectorDb},
    error::{Error, Result},
    indexer::{
        IndexHook, SyncReport, SyncWithHookError, build_document_from_disk, file_stamp,
        is_indexable_path, path_to_doc_id, sync_workspace, sync_workspace_full_with_hook,
        sync_workspace_incremental, sync_workspace_with_hook,
    },
    workspace::Workspace,
};

use sapphire_retrieve::build_embedder;

/// Controls which retrieval strategy [`WorkspaceState::retrieve_files`] uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// mtime/size-based change-detection store (see [`sapphire_track`]). Unlike the
    /// retrieve backend it is never swapped at runtime, so it needs no lock.
    track_db: Arc<dyn TrackStore + Send + Sync>,
    embedder: OnceCell<Option<Box<dyn Embedder + Send + Sync>>>,
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

/// Whether `retrieve` is empty while `track` still records stamps.
///
/// That pair is never consistent. It is what a reset of the retrieve store
/// leaves behind (`RedbStore::open` wiping an old schema or an
/// `UpgradeRequired` file, or the store directory deleted by hand). An
/// incremental sync would skip every file whose stamp matches and the index
/// would stay empty, so the caller clears the track store.
fn index_lost_its_documents(
    retrieve: &(dyn RetrieveStore + Send + Sync),
    track: &(dyn TrackStore + Send + Sync),
) -> Result<bool> {
    Ok(retrieve.document_count()? == 0 && track.count()? > 0)
}

/// Convert a `sapphire_retrieve::Error` to a `SyncWithHookError::Workspace`.
fn map_retrieve_err<E: std::error::Error + Send + Sync + 'static>(
    e: sapphire_retrieve::Error,
) -> SyncWithHookError<E> {
    SyncWithHookError::Workspace(Error::from(e))
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
            // `Path::join` with an empty path appends a trailing separator, so
            // seed `suffix` with `name` on the first iteration instead.
            suffix = if suffix.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                Path::new(name).join(&suffix)
            };
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
    pub fn open(workspace: Workspace) -> Result<Self> {
        let backend = Self::open_initial_backend(&workspace)?;
        let mut track_db = Self::open_initial_track(&workspace)?;
        if index_lost_its_documents(backend.as_ref(), track_db.as_ref())? {
            // The retrieve store was reset (an old schema or redb file format)
            // but the track store still has a stamp for every file, so an
            // incremental sync would skip them all. Start the track store over,
            // as `rebuild` does, so the next sync re-indexes the workspace.
            drop(track_db);
            let _ = std::fs::remove_file(workspace.track_db_path());
            track_db = Self::open_initial_track(&workspace)?;
        }
        Ok(Self {
            retrieve_db: Mutex::new(backend),
            track_db,
            workspace,
            embedder: OnceCell::new(),
        })
    }

    /// Delete and recreate the retrieve DB from scratch.
    pub fn rebuild(workspace: Workspace) -> Result<Self> {
        // Drop the change-detection snapshot too, so the rebuilt retrieve
        // index and the track store start from a consistent (empty) state.
        // The orphaned pre-#118 `track_v1.redb` goes with it, so a rebuild
        // also clears any stale second-resolution snapshot.
        let _ = std::fs::remove_file(workspace.track_db_path());
        let _ = std::fs::remove_file(workspace.cache_dir().join("track_v1.redb"));
        let backend = Self::open_initial_backend(&workspace)?;
        let track_db = Self::open_initial_track(&workspace)?;
        Ok(Self {
            retrieve_db: Mutex::new(backend),
            track_db,
            workspace,
            embedder: OnceCell::new(),
        })
    }

    /// Clone the active retrieve backend as an `Arc<dyn RetrieveStore>`.
    ///
    /// The lock is released immediately after cloning the `Arc`, so long-running
    /// operations do not block other threads from checking the backend state.
    pub fn retrieve_db(&self) -> Arc<dyn RetrieveStore + Send + Sync> {
        Arc::clone(&*self.retrieve_db.lock().unwrap())
    }

    /// Borrow the mtime change-detection store.
    pub fn track_db(&self) -> &(dyn TrackStore + Send + Sync) {
        self.track_db.as_ref()
    }

    pub fn embedder(&self) -> Option<&dyn Embedder> {
        Some(self.embedder.get()?.as_ref()?.as_ref())
    }

    // ── single-file update API ────────────────────────────────────────────────

    /// Update the retrieve index for a single file.
    ///
    /// Reads the file from disk and upserts it into the retrieve DB. The file
    /// is indexed whole, whatever its extension: when it changes, the whole
    /// file is re-indexed and re-embedded.
    pub fn on_file_updated(&self, path: &Path) -> Result<()> {
        let resolved = self.resolve_path(path)?;
        if !resolved.is_internal() {
            return Ok(());
        }
        let abs = resolved.as_path();
        let path_str = abs.to_string_lossy().into_owned();

        let stamp = file_stamp(abs);

        let doc = build_document_from_disk(abs, path_to_doc_id(abs))?;

        // Index first, then record the stamp (see the atomicity note in
        // `indexer::sync_inner`).
        let db = self.retrieve_db();
        db.upsert_document(&doc)?;
        db.rebuild_fts()?;
        self.track_db().upsert(&path_str, stamp)?;

        Ok(())
    }

    /// Remove a file from the retrieve index.
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
        db.rebuild_fts()?;
        self.track_db().remove(&path_str)?;

        Ok(())
    }

    // ── hook-aware single-file API ────────────────────────────────────────────

    /// Like [`on_file_updated`](Self::on_file_updated) but invokes
    /// `hook.on_changed` immediately before the retrieve DB is updated, so a
    /// caller (e.g. sapphire-journal) can update its own per-file caches in
    /// lockstep with the workspace.
    ///
    /// The hook does **not** see or modify the indexed [`Document`]; the
    /// workspace always reads the file from disk and indexes it whole.
    /// Non-indexable extensions and external paths short-circuit
    /// without invoking the hook.
    pub fn on_file_updated_with_hook<H: IndexHook>(
        &self,
        path: &Path,
        hook: &mut H,
    ) -> std::result::Result<(), SyncWithHookError<H::Error>> {
        let resolved = self
            .resolve_path(path)
            .map_err(SyncWithHookError::Workspace)?;
        if !resolved.is_internal() {
            return Ok(());
        }
        let abs = resolved.as_path();
        if !is_indexable_path(abs) {
            return Ok(());
        }
        let path_str = abs.to_string_lossy().into_owned();
        let stamp = file_stamp(abs);

        hook.on_changed(abs, stamp.mtime_ns)
            .map_err(SyncWithHookError::Hook)?;

        let doc_id = path_to_doc_id(abs);
        let doc = build_document_from_disk(abs, doc_id)
            .map_err(|e| SyncWithHookError::Workspace(Error::from(e)))?;

        let db = self.retrieve_db();
        db.upsert_document(&doc).map_err(map_retrieve_err)?;
        db.rebuild_fts().map_err(map_retrieve_err)?;
        self.track_db()
            .upsert(&path_str, stamp)
            .map_err(|e| SyncWithHookError::Workspace(Error::from(e)))?;

        Ok(())
    }

    /// Like [`on_file_deleted`](Self::on_file_deleted) but invokes
    /// `hook.on_removed` immediately before the retrieve DB rows are deleted,
    /// so the caller can clean up its own caches in lockstep.
    pub fn on_file_deleted_with_hook<H: IndexHook>(
        &self,
        path: &Path,
        hook: &mut H,
    ) -> std::result::Result<(), SyncWithHookError<H::Error>> {
        let resolved = self
            .resolve_path(path)
            .map_err(SyncWithHookError::Workspace)?;
        if !resolved.is_internal() {
            return Ok(());
        }
        let abs = resolved.as_path();
        let path_str = abs.to_string_lossy().into_owned();
        let doc_id = path_to_doc_id(abs);

        hook.on_removed(&path_str)
            .map_err(SyncWithHookError::Hook)?;

        let db = self.retrieve_db();
        db.remove_document(doc_id).map_err(map_retrieve_err)?;
        db.rebuild_fts().map_err(map_retrieve_err)?;
        self.track_db()
            .remove(&path_str)
            .map_err(|e| SyncWithHookError::Workspace(Error::from(e)))?;

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
            if index_lost_its_documents(backend.as_ref(), self.track_db())? {
                // Same rule as in `open`. The track store is shared without a
                // lock and stays open here, so its entries are removed in place
                // instead of deleting the file.
                let track = self.track_db();
                for path in track.mtimes()?.keys() {
                    track.remove(path)?;
                }
            }
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
        sync_workspace(&self.workspace, self.retrieve_db(), self.track_db())
    }

    /// Run a mtime/size-based incremental retrieve cache refresh.
    ///
    /// Only re-indexes files whose recorded mtime or size has changed since
    /// the last run.
    /// Does **not** perform any git sync.
    ///
    /// Returns `(upserted, removed)`.
    pub fn sync_retrieve(&self) -> Result<(usize, usize)> {
        sync_workspace_incremental(&self.workspace, self.retrieve_db(), self.track_db())
    }

    /// Hook-aware full sync (counterpart of [`sync`](Self::sync)).
    ///
    /// Re-indexes every file regardless of mtime and invokes `hook.on_changed`
    /// / `hook.on_removed` / `hook.after_sweep` so the caller can update its
    /// own per-file caches in lockstep with the workspace's retrieve DB.
    pub fn sync_with_hook<H: IndexHook>(
        &self,
        hook: &mut H,
    ) -> std::result::Result<SyncReport, SyncWithHookError<H::Error>> {
        sync_workspace_full_with_hook(&self.workspace, self.retrieve_db(), self.track_db(), hook)
    }

    /// Hook-aware incremental sync (counterpart of
    /// [`sync_retrieve`](Self::sync_retrieve)).
    ///
    /// Skips files whose recorded stamp (mtime and size) matches the cached
    /// value; the hook is only invoked for new / changed / removed paths.
    pub fn sync_retrieve_with_hook<H: IndexHook>(
        &self,
        hook: &mut H,
    ) -> std::result::Result<SyncReport, SyncWithHookError<H::Error>> {
        sync_workspace_with_hook(&self.workspace, self.retrieve_db(), self.track_db(), hook)
    }

    /// Sync and, when embedding is configured, embed pending documents.
    ///
    /// Returns `(upserted, removed, embedded)`.
    pub async fn sync_and_embed(&self, retrieve: &RetrieveConfig) -> Result<(usize, usize, usize)> {
        let (upserted, removed) =
            sync_workspace(&self.workspace, self.retrieve_db(), self.track_db())?;

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

    /// Embed all pending documents (sync). Loads backend and embedder if needed.
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
    ///
    /// Priority: pure-Rust redb+tantivy (`redb-store`, default) → ephemeral
    /// in-memory.
    fn open_initial_backend(workspace: &Workspace) -> Result<Arc<dyn RetrieveStore + Send + Sync>> {
        #[cfg(feature = "redb-store")]
        {
            return Ok(open_redb(&workspace.retrieve_db_path())?);
        }
        #[cfg(not(feature = "redb-store"))]
        {
            let _ = workspace;
            Ok(sapphire_retrieve::open_in_memory())
        }
    }

    /// Create the mtime change-detection store appropriate for the compiled
    /// features.
    ///
    /// Mirrors [`open_initial_backend`](Self::open_initial_backend): a
    /// persistent redb store when a persistent retrieve backend is in use
    /// (`redb-store`), otherwise an ephemeral in-memory store so the two
    /// never drift (a persistent mtime snapshot paired with an empty in-memory
    /// index would make changed files look unchanged).
    fn open_initial_track(workspace: &Workspace) -> Result<Arc<dyn TrackStore + Send + Sync>> {
        #[cfg(feature = "redb-store")]
        {
            Ok(Arc::new(sapphire_track::open_redb(
                &workspace.track_db_path(),
            )?))
        }
        #[cfg(not(feature = "redb-store"))]
        {
            let _ = workspace;
            Ok(Arc::new(sapphire_track::open_in_memory()))
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
            #[cfg(feature = "redb-store")]
            VectorDb::Redb => Ok(Some(open_redb_vec(
                &self.workspace.retrieve_db_path(),
                dim,
            )?)),
            #[cfg(not(feature = "redb-store"))]
            VectorDb::Redb => Err(crate::error::Error::RedbStoreNotEnabled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "redb-store")]
    mod hook {
        use super::super::*;
        use crate::AppContext;
        use std::cell::Cell;
        use std::fs;
        use std::path::PathBuf;

        fn ctx() -> &'static AppContext {
            static CTX: std::sync::OnceLock<AppContext> = std::sync::OnceLock::new();
            CTX.get_or_init(|| AppContext::new("ws-state-hook-test"))
        }

        fn make_state() -> (tempfile::TempDir, WorkspaceState) {
            // Avoid dotted prefix: the workspace walker filters dotted dirs at
            // any depth including the root.
            let tmp = tempfile::Builder::new().prefix("ws-").tempdir().unwrap();
            // Each tempdir gets a fresh cache root: setting `set_cache_dir` is
            // first-writer-wins, so we accept whatever the first test puts
            // there. Use a process-wide tmp subdir as the shared cache.
            ctx().set_cache_dir(std::env::temp_dir().join("ws-state-hook-cache"));
            fs::create_dir_all(tmp.path().join(".ws-state-hook-test")).unwrap();
            let ws = Workspace::from_root(ctx(), tmp.path()).unwrap();
            let state = WorkspaceState::open(ws).unwrap();
            (tmp, state)
        }

        struct RecordingHook {
            changed: Vec<PathBuf>,
            removed: Vec<String>,
            after_sweep_count: Cell<usize>,
        }
        impl RecordingHook {
            fn new() -> Self {
                Self {
                    changed: Vec::new(),
                    removed: Vec::new(),
                    after_sweep_count: Cell::new(0),
                }
            }
        }
        impl IndexHook for RecordingHook {
            type Error = std::convert::Infallible;
            fn on_changed(&mut self, p: &Path, _m: i64) -> std::result::Result<(), Self::Error> {
                self.changed.push(p.to_path_buf());
                Ok(())
            }
            fn on_removed(&mut self, p: &str) -> std::result::Result<(), Self::Error> {
                self.removed.push(p.to_owned());
                Ok(())
            }
            fn after_sweep(&mut self) -> std::result::Result<(), Self::Error> {
                self.after_sweep_count.set(self.after_sweep_count.get() + 1);
                Ok(())
            }
        }

        #[test]
        fn sync_retrieve_with_hook_invokes_hook_per_changed_file() {
            let (tmp, state) = make_state();
            fs::write(tmp.path().join("a.md"), "a").unwrap();
            fs::write(tmp.path().join("b.md"), "b").unwrap();

            let mut hook = RecordingHook::new();
            let report = state.sync_retrieve_with_hook(&mut hook).unwrap();

            assert_eq!(report.upserted, 2);
            assert_eq!(report.removed, 0);
            assert_eq!(hook.changed.len(), 2);
            assert_eq!(hook.after_sweep_count.get(), 1);
        }

        #[test]
        fn sync_retrieve_with_hook_skips_unchanged_files() {
            let (tmp, state) = make_state();
            fs::write(tmp.path().join("stable.md"), "x").unwrap();

            let mut h1 = RecordingHook::new();
            state.sync_retrieve_with_hook(&mut h1).unwrap();
            assert_eq!(h1.changed.len(), 1);

            let mut h2 = RecordingHook::new();
            let r = state.sync_retrieve_with_hook(&mut h2).unwrap();
            assert_eq!(h2.changed.len(), 0);
            assert_eq!(r.upserted, 0);
        }

        #[test]
        fn sync_with_hook_runs_full_re_index() {
            let (tmp, state) = make_state();
            fs::write(tmp.path().join("stable.md"), "x").unwrap();

            // Prime the incremental cache so mtimes are recorded.
            let mut h1 = RecordingHook::new();
            state.sync_retrieve_with_hook(&mut h1).unwrap();

            // Full sync must invoke the hook even though nothing changed.
            let mut h2 = RecordingHook::new();
            let r = state.sync_with_hook(&mut h2).unwrap();
            assert_eq!(h2.changed.len(), 1);
            assert_eq!(r.upserted, 1);
        }

        #[test]
        fn on_file_updated_with_hook_indexes_and_fires_hook() {
            let (tmp, state) = make_state();
            let file = tmp.path().join("note.md");
            fs::write(&file, "body").unwrap();

            let mut hook = RecordingHook::new();
            state.on_file_updated_with_hook(&file, &mut hook).unwrap();

            assert_eq!(hook.changed, vec![file.canonicalize().unwrap()]);
            assert_eq!(state.retrieve_db().document_count().unwrap(), 1);
        }

        #[test]
        fn on_file_updated_with_hook_skips_non_indexable_extension() {
            let (tmp, state) = make_state();
            let file = tmp.path().join("blob.bin");
            fs::write(&file, "data").unwrap();

            let mut hook = RecordingHook::new();
            state.on_file_updated_with_hook(&file, &mut hook).unwrap();

            assert!(hook.changed.is_empty());
            assert_eq!(state.retrieve_db().document_count().unwrap(), 0);
        }

        #[test]
        fn on_file_deleted_with_hook_calls_hook_then_removes() {
            let (tmp, state) = make_state();
            let file = tmp.path().join("doomed.md");
            fs::write(&file, "bye").unwrap();
            state
                .on_file_updated_with_hook(&file, &mut RecordingHook::new())
                .unwrap();
            assert_eq!(state.retrieve_db().document_count().unwrap(), 1);

            let mut hook = RecordingHook::new();
            state.on_file_deleted_with_hook(&file, &mut hook).unwrap();

            assert_eq!(hook.removed.len(), 1);
            assert!(
                hook.removed[0].ends_with("doomed.md"),
                "got {:?}",
                hook.removed[0]
            );
            assert_eq!(state.retrieve_db().document_count().unwrap(), 0);
        }

        fn reopen(tmp: &tempfile::TempDir) -> WorkspaceState {
            WorkspaceState::open(Workspace::from_root(ctx(), tmp.path()).unwrap()).unwrap()
        }

        #[test]
        fn a_wiped_retrieve_store_is_reindexed_by_an_incremental_sync() {
            let (tmp, state) = make_state();
            fs::write(tmp.path().join("a.md"), "alpha").unwrap();
            fs::write(tmp.path().join("b.md"), "bravo").unwrap();
            state.sync().unwrap();
            assert_eq!(state.retrieve_db().document_count().unwrap(), 2);
            let store_dir = state.workspace.retrieve_db_path().with_extension("redb");
            drop(state);

            // Simulate the reset `RedbStore::open` performs for an old schema or an
            // `UpgradeRequired` file: the retrieve store is gone, the track store is not.
            fs::remove_dir_all(&store_dir).unwrap();

            let state = reopen(&tmp);
            let (upserted, _) = state.sync_retrieve().unwrap();

            assert_eq!(upserted, 2);
            assert_eq!(state.retrieve_db().document_count().unwrap(), 2);
        }

        #[test]
        fn reopening_a_populated_store_keeps_the_track_store() {
            let (tmp, state) = make_state();
            fs::write(tmp.path().join("a.md"), "alpha").unwrap();
            state.sync().unwrap();
            drop(state);

            let state = reopen(&tmp);

            assert_eq!(state.track_db().count().unwrap(), 1);
            let (upserted, _) = state.sync_retrieve().unwrap();
            assert_eq!(upserted, 0, "unchanged files are still skipped");
        }
    }

    /// Regression for #48: `canonicalize_or_parent` previously returned a path
    /// with a trailing separator for not-yet-existing files, which caused
    /// `std::fs::write` to fail with `EISDIR` when creating a new file.
    #[test]
    fn canonicalize_or_parent_no_trailing_separator_for_new_file() {
        let tmp = std::env::temp_dir()
            .canonicalize()
            .expect("temp dir canonicalizes");
        let unique = format!(
            "sapphire-workspace-test-{}-{}.md",
            std::process::id(),
            uuid::Uuid::now_v7()
        );
        let new_file = tmp.join(&unique);
        assert!(!new_file.exists(), "fixture path should not exist");

        let resolved = canonicalize_or_parent(&new_file).expect("resolves");

        assert_eq!(
            resolved.file_name().and_then(|s| s.to_str()),
            Some(unique.as_str()),
            "file_name must survive the walk-up: got {resolved:?}",
        );
        assert_eq!(resolved.parent(), Some(tmp.as_path()));

        // The canonical bug signature: a trailing separator made `std::fs::write`
        // fail with `IsADirectory`. Writing to the resolved path must succeed.
        std::fs::write(&resolved, b"hello").expect("write must succeed");
        std::fs::remove_file(&resolved).ok();
    }

    #[test]
    fn canonicalize_or_parent_handles_multiple_missing_components() {
        let tmp = std::env::temp_dir()
            .canonicalize()
            .expect("temp dir canonicalizes");
        let unique = format!(
            "sapphire-workspace-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        );
        let nested = tmp.join(&unique).join("sub").join("leaf.md");

        let resolved = canonicalize_or_parent(&nested).expect("resolves");

        assert!(resolved.starts_with(&tmp));
        assert_eq!(
            resolved.file_name().and_then(|s| s.to_str()),
            Some("leaf.md"),
            "innermost component must be preserved: got {resolved:?}",
        );
        let bytes = resolved.as_os_str().as_encoded_bytes();
        assert!(
            !bytes.ends_with(b"/") && !bytes.ends_with(b"\\"),
            "resolved path must not have a trailing separator: got {resolved:?}",
        );
    }
}
