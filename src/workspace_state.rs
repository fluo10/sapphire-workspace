use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[cfg(feature = "sqlite-store")]
use sapphire_retrieve::db::SCHEMA_VERSION;
use sapphire_retrieve::{Document, Embedder, RetrieveDb, SearchResult};
use tokio::sync::OnceCell;

use crate::{
    config::{HybridConfig, UserConfig, VectorDb, WorkspaceConfig},
    error::Result,
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
    retrieve_db: RetrieveDb,
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

impl WorkspaceState {
    /// Open (or create) the retrieve DB for `workspace`.
    ///
    /// When the `git-sync` feature is enabled, automatically attaches a
    /// [`sapphire_sync::GitSync`] backend if the workspace root is inside a
    /// git repository.  Silently falls back to no backend if git is not found.
    pub fn open(workspace: Workspace) -> Result<Self> {
        let retrieve_db = RetrieveDb::open(&workspace.retrieve_db_path())?;
        let mut state = Self {
            workspace,
            retrieve_db,
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
        let retrieve_db = RetrieveDb::rebuild(&workspace.retrieve_db_path())?;
        #[cfg(feature = "lancedb-store")]
        {
            use sapphire_retrieve::lancedb_store;
            let _ = std::fs::remove_dir_all(lancedb_store::data_dir(&workspace.cache_dir()));
        }
        Ok(Self {
            workspace,
            retrieve_db,
            embedder: OnceCell::new(),
            sync_backend: None,
        })
    }

    /// Open workspace and configure the sync backend from [`WorkspaceConfig`].
    ///
    /// - `SyncBackendKind::Auto` (default) — same as [`open`](Self::open):
    ///   attach git if a repository is found, silently no-op otherwise.
    /// - `SyncBackendKind::Git` — attach git with the configured remote;
    ///   returns an error if no repository is found.
    /// - `SyncBackendKind::None` — disable sync even inside a git repository.
    #[cfg(feature = "git-sync")]
    pub fn open_configured(workspace: Workspace, config: &WorkspaceConfig) -> Result<Self> {
        use crate::config::SyncBackendKind;
        let mut state = Self::open(workspace)?;
        match config.sync.backend {
            SyncBackendKind::Auto => {
                // Re-create the backend so we can apply the device_id commit message.
                if let Ok(git) = sapphire_sync::GitSync::open(&state.workspace.root) {
                    state.set_sync_backend(Box::new(Self::apply_device_id(git, &config.sync)));
                }
            }
            SyncBackendKind::Git => {
                // Explicit git: use the configured remote and fail hard if
                // no repository is found.
                let git = sapphire_sync::GitSync::with_remote(
                    &state.workspace.root,
                    config.sync.remote(),
                )?;
                state.set_sync_backend(Box::new(Self::apply_device_id(git, &config.sync)));
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
        match sync.device_id {
            Some(id) => git.with_commit_message(format!("auto: sync [{id}]")),
            None => git,
        }
    }

    /// Open workspace and configure the sync backend from [`WorkspaceConfig`].
    /// (no-op version when the `git-sync` feature is not compiled in)
    #[cfg(not(feature = "git-sync"))]
    pub fn open_configured(workspace: Workspace, _config: &WorkspaceConfig) -> Result<Self> {
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

    pub fn retrieve_db(&self) -> &RetrieveDb {
        &self.retrieve_db
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
        let path_str = path.to_string_lossy().into_owned();

        let mtime = path
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            })
            .unwrap_or(0);

        let body = std::fs::read_to_string(path)?;
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let doc_id = path_to_doc_id(path);

        self.retrieve_db.upsert_file(&path_str, mtime)?;
        self.retrieve_db.upsert_document(&Document {
            id: doc_id,
            title,
            body,
            path: path_str,
            chunks: None,
        })?;
        self.retrieve_db.rebuild_fts()?;

        if let Some(sync) = &self.sync_backend {
            sync.add_file(path)?;
        }

        Ok(())
    }

    /// Remove a file from the retrieve index and unstage it via the sync
    /// backend (if configured).
    pub fn on_file_deleted(&self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy().into_owned();
        let doc_id = path_to_doc_id(path);

        self.retrieve_db.remove_document(doc_id)?;
        self.retrieve_db.remove_file(&path_str)?;
        self.retrieve_db.rebuild_fts()?;

        if let Some(sync) = &self.sync_backend {
            sync.remove_file(path)?;
        }

        Ok(())
    }

    // ── workspace-relative file operations ───────────────────────────────────
    //
    // These methods accept paths relative to the workspace root, perform the
    // corresponding filesystem operation, and then update the retrieve index
    // (and sync backend) automatically.

    /// Read a workspace-relative text file and return its contents as a `String`.
    pub fn read_file(&self, relative: &Path) -> Result<String> {
        let abs = self.workspace.root.join(relative);
        Ok(std::fs::read_to_string(&abs)?)
    }

    /// Read a line range from a workspace-relative text file.
    ///
    /// `start_line` and `end_line` are **1-indexed** and **inclusive**.
    /// `end_line: None` reads to the end of the file.
    /// Lines beyond the end of the file are silently clamped.
    pub fn read_file_range(
        &self,
        relative: &Path,
        start_line: usize,
        end_line: Option<usize>,
    ) -> Result<String> {
        let abs = self.workspace.root.join(relative);
        let content = std::fs::read_to_string(&abs)?;
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

    /// List the direct children of a workspace-relative directory.
    ///
    /// Returns pairs of `(workspace-relative path, is_dir)`, sorted
    /// alphabetically by path.
    pub fn list_dir(&self, relative: &Path) -> Result<Vec<(PathBuf, bool)>> {
        let abs = self.workspace.root.join(relative);
        let mut entries: Vec<(PathBuf, bool)> = std::fs::read_dir(&abs)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let rel = e.path().strip_prefix(&self.workspace.root).ok()?.to_owned();
                Some((rel, is_dir))
            })
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Write `content` to a workspace-relative file and update the index.
    ///
    /// Creates any missing parent directories automatically.
    /// Overwrites the file if it already exists.
    pub fn write_file(&self, relative: &Path, content: &str) -> Result<()> {
        let abs = self.workspace.root.join(relative);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, content)?;
        self.on_file_updated(&abs)
    }

    /// Append `content` to a workspace-relative file and update the index.
    ///
    /// Creates the file (and any missing parent directories) if it does not
    /// exist yet.
    pub fn append_file(&self, relative: &Path, content: &str) -> Result<()> {
        use std::io::Write as _;
        let abs = self.workspace.root.join(relative);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&abs)?;
        file.write_all(content.as_bytes())?;
        drop(file);
        self.on_file_updated(&abs)
    }

    /// Delete a workspace-relative file from disk and remove it from the index.
    pub fn delete_file(&self, relative: &Path) -> Result<()> {
        let abs = self.workspace.root.join(relative);
        std::fs::remove_file(&abs)?;
        self.on_file_deleted(&abs)
    }

    // ── vector backend ────────────────────────────────────────────────────────

    /// Initialise the vector backend (sync). Idempotent.
    pub fn load_retrieve_backend(&self, config: &UserConfig) -> Result<()> {
        let Some(retrieve) = &config.retrieve else {
            return Ok(());
        };
        let Some(embed_cfg) = &retrieve.embedding else {
            return Ok(());
        };
        if !embed_cfg.enabled {
            return Ok(());
        }
        let Some(dim) = embed_cfg.dimension else {
            return Ok(());
        };
        self.init_vector_backend(retrieve.db, dim)
    }

    /// Async version of [`load_retrieve_backend`](Self::load_retrieve_backend).
    pub async fn load_retrieve_backend_async(&self, config: &UserConfig) -> Result<()> {
        let Some(retrieve) = &config.retrieve else {
            return Ok(());
        };
        let Some(embed_cfg) = &retrieve.embedding else {
            return Ok(());
        };
        if !embed_cfg.enabled {
            return Ok(());
        }
        let Some(dim) = embed_cfg.dimension else {
            return Ok(());
        };
        let vector_db = retrieve.db;

        #[cfg(feature = "lancedb-store")]
        if vector_db == VectorDb::LanceDb {
            use sapphire_retrieve::lancedb_store;
            let lancedb_dir = lancedb_store::data_dir(&self.workspace.cache_dir());
            self.retrieve_db.init_lancedb(&lancedb_dir, dim)?;
            return Ok(());
        }

        self.init_vector_backend(vector_db, dim)
    }

    fn init_vector_backend(&self, vector_db: VectorDb, dim: u32) -> Result<()> {
        match vector_db {
            VectorDb::None => {}
            #[cfg(feature = "sqlite-store")]
            VectorDb::SqliteVec => {
                self.retrieve_db.init_sqlite_vec(dim)?;
            }
            #[cfg(not(feature = "sqlite-store"))]
            VectorDb::SqliteVec => {
                return Err(crate::error::Error::SqliteStoreNotEnabled);
            }
            #[cfg(feature = "lancedb-store")]
            VectorDb::LanceDb => {
                use sapphire_retrieve::lancedb_store;
                let lancedb_dir = lancedb_store::data_dir(&self.workspace.cache_dir());
                self.retrieve_db.init_lancedb(&lancedb_dir, dim)?;
            }
            #[cfg(not(feature = "lancedb-store"))]
            VectorDb::LanceDb => {
                return Err(crate::error::Error::LanceDbNotEnabled);
            }
        }
        Ok(())
    }

    // ── embedder ──────────────────────────────────────────────────────────────

    /// Initialise the embedder (sync). Idempotent.
    pub fn load_embedder(&self, config: &UserConfig) -> Result<()> {
        if self.embedder.initialized() {
            return Ok(());
        }
        let embedder = config
            .retrieve
            .as_ref()
            .and_then(|r| r.embedding.as_ref())
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
    pub async fn load_embedder_async(&self, config: &UserConfig) -> Result<()> {
        let model_cache_dir = self.workspace.ctx.model_cache_dir();
        self.embedder
            .get_or_try_init(|| async {
                config
                    .retrieve
                    .as_ref()
                    .and_then(|r| r.embedding.as_ref())
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
        sync_workspace(&self.workspace, &self.retrieve_db)
    }

    /// Run the periodic sync cycle: git sync (if configured) followed by an
    /// mtime-based incremental cache update.
    ///
    /// This is the intended entry point for interval-based background
    /// refreshes.  It first synchronises with the remote (commit → pull →
    /// push), then walks the workspace and re-indexes only the files whose
    /// mtime has changed since the last run.
    ///
    /// Returns `(upserted, removed)`.
    pub fn periodic_sync(&self) -> Result<(usize, usize)> {
        if let Some(backend) = &self.sync_backend {
            backend.sync()?;
        }
        sync_workspace_incremental(&self.workspace, &self.retrieve_db)
    }

    /// Sync and, when embedding is configured, embed pending chunks.
    ///
    /// Returns `(upserted, removed, embedded)`.
    pub async fn sync_and_embed(&self, config: &UserConfig) -> Result<(usize, usize, usize)> {
        let (upserted, removed) = sync_workspace(&self.workspace, &self.retrieve_db)?;

        let embed_cfg = config.retrieve.as_ref().and_then(|r| r.embedding.as_ref());
        let Some(embed_cfg) = embed_cfg else {
            return Ok((upserted, removed, 0));
        };
        if !embed_cfg.enabled {
            return Ok((upserted, removed, 0));
        }

        self.load_retrieve_backend_async(config).await?;
        self.load_embedder_async(config).await?;

        let Some(embedder) = self.embedder() else {
            return Ok((upserted, removed, 0));
        };

        let embedded = self.retrieve_db.embed_pending(embedder, |_, _| {})?;
        Ok((upserted, removed, embedded))
    }

    /// Embed all pending chunks (sync). Loads backend and embedder if needed.
    pub fn embed_pending(
        &self,
        config: &UserConfig,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        let embed_cfg = config.retrieve.as_ref().and_then(|r| r.embedding.as_ref());
        let Some(embed_cfg) = embed_cfg else {
            return Ok(0);
        };
        if !embed_cfg.enabled {
            return Ok(0);
        }
        self.load_retrieve_backend(config)?;
        self.load_embedder(config)?;
        let Some(embedder) = self.embedder() else {
            return Ok(0);
        };
        Ok(self.retrieve_db.embed_pending(embedder, on_progress)?)
    }

    // ── info ──────────────────────────────────────────────────────────────────

    pub fn db_info(&self) -> Result<DbInfo> {
        let db_path = self.workspace.retrieve_db_path();
        let document_count = self.retrieve_db.document_count().unwrap_or(0);
        let vec_info = self
            .retrieve_db
            .vec_info()
            .unwrap_or(sapphire_retrieve::VecInfo {
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
    ) -> Result<Vec<SearchResult>> {
        // Over-fetch to compensate for post-filter losses.
        let fetch_limit = if params.folder.is_some() {
            params.limit * 3
        } else {
            params.limit
        };

        // Fall back to FTS when the embedder is not available.
        let effective_mode = match params.mode {
            SearchMode::Semantic | SearchMode::Hybrid if self.embedder().is_none() => {
                SearchMode::Fts
            }
            other => other,
        };

        let results = match effective_mode {
            SearchMode::Fts => self.search_fts_internal(params.query, fetch_limit)?,
            SearchMode::Semantic => self.search_semantic_internal(params.query, fetch_limit)?,
            SearchMode::Hybrid => {
                self.search_hybrid_internal(params.query, fetch_limit, hybrid_config)?
            }
        };

        let results = Self::filter_by_folder(results, params.folder);
        Ok(results.into_iter().take(params.limit).collect())
    }

    fn search_fts_internal(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        Ok(self.retrieve_db.search_fts(query, limit)?)
    }

    fn search_semantic_internal(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let embedder = self.embedder().expect("caller verified embedder exists");
        let query_vecs = embedder.embed_texts(&[query])?;
        let query_vec = query_vecs.into_iter().next().ok_or_else(|| {
            sapphire_retrieve::Error::Embed("embedder returned empty result".into())
        })?;
        let raw = self.retrieve_db.search_similar(&query_vec, limit * 3)?;
        Ok(RetrieveDb::dedup_chunk_results(raw, limit))
    }

    fn search_hybrid_internal(
        &self,
        query: &str,
        limit: usize,
        config: &HybridConfig,
    ) -> Result<Vec<SearchResult>> {
        let fts_results = self.search_fts_internal(query, limit)?;
        let sem_results = self.search_semantic_internal(query, limit)?;
        Ok(Self::merge_rrf(fts_results, sem_results, limit, config))
    }

    /// Merge two ranked result lists via Reciprocal Rank Fusion.
    ///
    /// `score(d) = w_fts / (k + rank_fts) + w_sem / (k + rank_sem)`
    ///
    /// Documents appearing in only one list receive a contribution from that
    /// list alone.  The returned list is sorted by descending RRF score.
    fn merge_rrf(
        fts: Vec<SearchResult>,
        semantic: Vec<SearchResult>,
        limit: usize,
        config: &HybridConfig,
    ) -> Vec<SearchResult> {
        let k = config.rrf_k as f64;
        let w_fts = config.fts_weight;
        let w_sem = 1.0 - w_fts;

        let mut scores: HashMap<String, (SearchResult, f64)> = HashMap::new();

        for (rank, result) in fts.into_iter().enumerate() {
            let rrf = w_fts / (k + (rank + 1) as f64);
            scores
                .entry(result.path.clone())
                .and_modify(|(_, s)| *s += rrf)
                .or_insert((result, rrf));
        }

        for (rank, result) in semantic.into_iter().enumerate() {
            let rrf = w_sem / (k + (rank + 1) as f64);
            scores
                .entry(result.path.clone())
                .and_modify(|(_, s)| *s += rrf)
                .or_insert((result, rrf));
        }

        let mut merged: Vec<_> = scores.into_values().collect();
        merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        merged.truncate(limit);

        merged
            .into_iter()
            .map(|(mut result, rrf_score)| {
                result.score = rrf_score;
                result
            })
            .collect()
    }

    fn filter_by_folder(results: Vec<SearchResult>, folder: Option<&Path>) -> Vec<SearchResult> {
        let Some(prefix) = folder else { return results };
        let prefix_str = prefix.to_string_lossy();
        results
            .into_iter()
            .filter(|r| r.path.starts_with(prefix_str.as_ref()))
            .collect()
    }
}
