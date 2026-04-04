use std::path::{Path, PathBuf};

use sapphire_retrieve::{Document, Embedder, RetrieveDb, db::SCHEMA_VERSION};
use tokio::sync::OnceCell;

use crate::{
    config::{UserConfig, VectorDb, WorkspaceConfig},
    error::{Error, Result},
    indexer::{path_to_doc_id, sync_workspace},
    workspace::Workspace,
};

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
    pub fn open(workspace: Workspace) -> Result<Self> {
        let retrieve_db = RetrieveDb::open(&workspace.retrieve_db_path())?;
        Ok(Self {
            workspace,
            retrieve_db,
            embedder: OnceCell::new(),
            sync_backend: None,
        })
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
    /// - `SyncBackendKind::Git` → initialises [`sapphire_sync::GitSync`] with the
    ///   remote name from `config.sync`.
    /// - `SyncBackendKind::None` → no sync backend (local-only).
    #[cfg(feature = "git-sync")]
    pub fn open_configured(workspace: Workspace, config: &WorkspaceConfig) -> Result<Self> {
        use crate::config::SyncBackendKind;
        let mut state = Self::open(workspace)?;
        if config.sync.backend == SyncBackendKind::Git {
            let git = sapphire_sync::GitSync::with_remote(
                &state.workspace.root,
                config.sync.remote(),
            )?;
            state.set_sync_backend(Box::new(git));
        }
        Ok(state)
    }

    /// Open workspace and configure the sync backend from [`WorkspaceConfig`].
    /// (no-op version when the `git-sync` feature is not compiled in)
    #[cfg(not(feature = "git-sync"))]
    pub fn open_configured(workspace: Workspace, _config: &WorkspaceConfig) -> Result<Self> {
        Self::open(workspace)
    }

    /// Borrow the sync backend, if one is configured.
    pub fn sync_backend(&self) -> Option<&dyn sapphire_sync::SyncBackend> {
        self.sync_backend.as_ref().map(|b| b.as_ref() as &dyn sapphire_sync::SyncBackend)
    }

    /// Attach a sync backend (e.g. `GitSync`).  Called once after construction.
    pub fn set_sync_backend(
        &mut self,
        backend: Box<dyn sapphire_sync::SyncBackend + Send + Sync>,
    ) {
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

    // ── vector backend ────────────────────────────────────────────────────────

    /// Initialise the vector backend (sync). Idempotent.
    pub fn load_retrieve_backend(&self, config: &UserConfig) -> Result<()> {
        let Some(embed_cfg) = &config.embedding else {
            return Ok(());
        };
        if !embed_cfg.enabled {
            return Ok(());
        }
        let Some(dim) = embed_cfg.dimension else {
            return Ok(());
        };
        self.init_vector_backend(embed_cfg.vector_db, dim)
    }

    /// Async version of [`load_retrieve_backend`](Self::load_retrieve_backend).
    pub async fn load_retrieve_backend_async(&self, config: &UserConfig) -> Result<()> {
        let Some(embed_cfg) = &config.embedding else {
            return Ok(());
        };
        if !embed_cfg.enabled {
            return Ok(());
        }
        let Some(dim) = embed_cfg.dimension else {
            return Ok(());
        };
        let vector_db = embed_cfg.vector_db;

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
            VectorDb::SqliteVec => {
                self.retrieve_db.init_sqlite_vec(dim)?;
            }
            #[cfg(feature = "lancedb-store")]
            VectorDb::LanceDb => {
                use sapphire_retrieve::lancedb_store;
                let lancedb_dir = lancedb_store::data_dir(&self.workspace.cache_dir());
                self.retrieve_db.init_lancedb(&lancedb_dir, dim)?;
            }
            #[cfg(not(feature = "lancedb-store"))]
            VectorDb::LanceDb => {
                return Err(Error::LanceDbNotEnabled);
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
            .embedding
            .as_ref()
            .filter(|c| c.enabled)
            .map(|c| sapphire_retrieve::build_embedder(&c.to_retrieve_embed_config()))
            .transpose()?;
        let _ = self.embedder.set(embedder);
        Ok(())
    }

    /// Async version of [`load_embedder`](Self::load_embedder).
    pub async fn load_embedder_async(&self, config: &UserConfig) -> Result<()> {
        self.embedder
            .get_or_try_init(|| async {
                config
                    .embedding
                    .as_ref()
                    .filter(|c| c.enabled)
                    .map(|c| sapphire_retrieve::build_embedder(&c.to_retrieve_embed_config()))
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

    /// Sync and, when embedding is configured, embed pending chunks.
    ///
    /// Returns `(upserted, removed, embedded)`.
    pub async fn sync_and_embed(&self, config: &UserConfig) -> Result<(usize, usize, usize)> {
        let (upserted, removed) = sync_workspace(&self.workspace, &self.retrieve_db)?;

        let Some(embed_cfg) = &config.embedding else {
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
        let Some(embed_cfg) = &config.embedding else {
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
            schema_version: SCHEMA_VERSION,
            document_count,
            embedding_dim: vec_info.embedding_dim,
            vector_count: vec_info.vector_count,
            pending_count: vec_info.pending_count,
        })
    }
}
