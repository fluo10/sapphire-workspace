//! Unified retrieve database: FTS5 + vector search.
//!
//! [`RetrieveDb`] is the main entry point.  It manages one of the available
//! storage backends and exposes a unified API for file tracking, document
//! management, full-text search, and vector search.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{
    embed::Embedder,
    error::Result,
    retrieve_store::{
        ChunkHit, Document, FileSearchResult, FtsQuery, HybridQuery, RetrieveStore, VectorQuery,
    },
    vector_store::VecInfo,
};

#[cfg(feature = "sqlite-store")]
use crate::sqlite_store::SqliteStore;

#[cfg(feature = "lancedb-store")]
use crate::lancedb_store::LanceDbBackend;

#[cfg(feature = "sqlite-store")]
pub use crate::sqlite_store::SCHEMA_VERSION;

// ── in-memory backend ─────────────────────────────────────────────────────────

/// In-memory backend used when no persistent storage feature is compiled in.
///
/// Data lives in `HashMap`s and is lost when the process exits.
struct InMemoryStore {
    state: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    files: HashMap<String, i64>,
    documents: HashMap<i64, Document>,
}

impl InMemoryStore {
    fn new() -> Self {
        Self {
            state: Mutex::new(InMemoryState::default()),
        }
    }
}

impl RetrieveStore for InMemoryStore {
    fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        Ok(self.state.lock().unwrap().files.clone())
    }

    fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(path.to_owned(), mtime);
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.state.lock().unwrap().files.remove(path);
        Ok(())
    }

    fn file_count(&self) -> Result<u64> {
        Ok(self.state.lock().unwrap().files.len() as u64)
    }

    fn upsert_document(&self, doc: &Document) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .documents
            .insert(doc.id, doc.clone());
        Ok(())
    }

    fn remove_document(&self, id: i64) -> Result<()> {
        self.state.lock().unwrap().documents.remove(&id);
        Ok(())
    }

    fn rebuild_fts(&self) -> Result<()> {
        Ok(())
    }

    fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<FileSearchResult>> {
        let state = self.state.lock().unwrap();
        let needle = q.query.to_lowercase();
        let prefix = q.path_prefix.map(|p| p.to_string_lossy().to_string());
        let mut results: Vec<FileSearchResult> = state
            .documents
            .values()
            .filter(|doc| {
                if let Some(ref pfx) = prefix {
                    if !doc.path.starts_with(pfx.as_str()) {
                        return false;
                    }
                }
                doc.title.to_lowercase().contains(&needle)
                    || doc.body.to_lowercase().contains(&needle)
            })
            .take(q.limit)
            .map(|doc| FileSearchResult {
                id: doc.id,
                title: doc.title.clone(),
                path: doc.path.clone(),
                score: 0.0,
                chunks: vec![ChunkHit {
                    line_start: 0,
                    line_end: 0,
                    text: doc.title.clone(),
                    score: 0.0,
                }],
            })
            .collect();
        results.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(results)
    }

    fn document_ids(&self) -> Result<Vec<i64>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .documents
            .keys()
            .copied()
            .collect())
    }

    fn document_count(&self) -> Result<u64> {
        Ok(self.state.lock().unwrap().documents.len() as u64)
    }

    fn embed_pending(
        &self,
        _embedder: &dyn Embedder,
        _on_progress: &dyn Fn(usize, usize),
    ) -> Result<usize> {
        Ok(0)
    }

    fn vec_info(&self) -> Result<VecInfo> {
        Ok(VecInfo {
            embedding_dim: 0,
            vector_count: 0,
            pending_count: 0,
        })
    }

    fn search_similar(&self, _q: &VectorQuery<'_>) -> Result<Vec<FileSearchResult>> {
        Ok(vec![])
    }
}

// ── backend state ─────────────────────────────────────────────────────────────

enum BackendState {
    #[allow(dead_code)]
    InMemory(Arc<InMemoryStore>),
    #[cfg(feature = "sqlite-store")]
    Sqlite(Arc<SqliteStore>),
    #[cfg(feature = "lancedb-store")]
    LanceDb(Arc<LanceDbBackend>),
}

impl BackendState {
    fn as_store(&self) -> Arc<dyn RetrieveStore> {
        match self {
            BackendState::InMemory(s) => Arc::clone(s) as Arc<dyn RetrieveStore>,
            #[cfg(feature = "sqlite-store")]
            BackendState::Sqlite(s) => Arc::clone(s) as Arc<dyn RetrieveStore>,
            #[cfg(feature = "lancedb-store")]
            BackendState::LanceDb(l) => Arc::clone(l) as Arc<dyn RetrieveStore>,
        }
    }

    fn needs_init(&self) -> bool {
        match self {
            BackendState::InMemory(_) => true,
            #[cfg(feature = "sqlite-store")]
            BackendState::Sqlite(s) => s.dim().is_none(),
            #[cfg(feature = "lancedb-store")]
            BackendState::LanceDb(_) => false,
        }
    }
}

// ── RetrieveDb ────────────────────────────────────────────────────────────────

pub struct RetrieveDb {
    db_path: PathBuf,
    backend: Mutex<BackendState>,
}

impl RetrieveDb {
    pub fn open(db_path: &Path) -> Result<Self> {
        #[cfg(feature = "sqlite-store")]
        {
            let store = SqliteStore::new_fts_only(db_path.to_owned());
            Ok(Self {
                db_path: db_path.to_owned(),
                backend: Mutex::new(BackendState::Sqlite(Arc::new(store))),
            })
        }

        #[cfg(not(feature = "sqlite-store"))]
        Ok(Self {
            db_path: db_path.to_owned(),
            backend: Mutex::new(BackendState::InMemory(Arc::new(InMemoryStore::new()))),
        })
    }

    pub fn rebuild(db_path: &Path) -> Result<Self> {
        #[cfg(feature = "sqlite-store")]
        crate::sqlite_store::wipe_db_files(db_path);
        Self::open(db_path)
    }

    #[cfg(feature = "sqlite-store")]
    pub fn init_sqlite_vec(&self, embedding_dim: u32) -> Result<()> {
        let mut guard = self.backend.lock().unwrap();
        if guard.needs_init() {
            let store = SqliteStore::new_with_vec(self.db_path.clone(), embedding_dim)?;
            *guard = BackendState::Sqlite(Arc::new(store));
        }
        Ok(())
    }

    #[cfg(feature = "lancedb-store")]
    pub fn init_lancedb(&self, lancedb_dir: &Path, embedding_dim: u32) -> Result<()> {
        let mut guard = self.backend.lock().unwrap();
        if guard.needs_init() {
            let backend = LanceDbBackend::new(lancedb_dir, embedding_dim)?;
            *guard = BackendState::LanceDb(Arc::new(backend));
        }
        Ok(())
    }

    fn store(&self) -> Arc<dyn RetrieveStore> {
        self.backend.lock().unwrap().as_store()
    }

    // ── document management ───────────────────────────────────────────────────

    pub fn upsert_document(&self, doc: &Document) -> Result<()> {
        self.store().upsert_document(doc)
    }

    pub fn remove_document(&self, id: i64) -> Result<()> {
        self.store().remove_document(id)
    }

    pub fn rebuild_fts(&self) -> Result<()> {
        self.store().rebuild_fts()
    }

    // ── search ────────────────────────────────────────────────────────────────

    pub fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<FileSearchResult>> {
        self.store().search_fts(q)
    }

    pub fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<FileSearchResult>> {
        self.store().search_similar(q)
    }

    pub fn search_hybrid(&self, q: &HybridQuery<'_>) -> Result<Vec<FileSearchResult>> {
        self.store().search_hybrid(q)
    }

    // ── embedding ─────────────────────────────────────────────────────────────

    pub fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        self.store().embed_pending(embedder, &on_progress)
    }

    pub fn vec_info(&self) -> Result<VecInfo> {
        self.store().vec_info()
    }

    pub fn document_ids(&self) -> Result<Vec<i64>> {
        self.store().document_ids()
    }

    pub fn document_count(&self) -> Result<u64> {
        self.store().document_count()
    }

    // ── file tracking ─────────────────────────────────────────────────────────

    pub fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        self.store().file_mtimes()
    }

    pub fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        self.store().upsert_file(path, mtime)
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.store().remove_file(path)
    }

    pub fn file_count(&self) -> Result<u64> {
        self.store().file_count()
    }
}

// ── free functions ────────────────────────────────────────────────────────────

/// Merge FTS and semantic file-level results via Reciprocal Rank Fusion.
///
/// `score(d) = w_fts / (k + rank_fts) + w_sem / (k + rank_sem)`.  Chunks from
/// both inputs are merged (deduplicated by `(line_start, line_end)`, keeping
/// the best per-chunk score).  Output is sorted by descending RRF score.
pub fn merge_rrf_files(
    fts: &[FileSearchResult],
    sem: &[FileSearchResult],
    k: f64,
    w_fts: f64,
    w_sem: f64,
    limit: usize,
) -> Vec<FileSearchResult> {
    // Index FTS results by path (stable id alternative would work too).
    let mut acc: HashMap<String, (FileSearchResult, f64)> = HashMap::new();

    for (rank, file) in fts.iter().enumerate() {
        let rrf = w_fts / (k + (rank + 1) as f64);
        acc.insert(file.path.clone(), (file.clone(), rrf));
    }

    for (rank, file) in sem.iter().enumerate() {
        let rrf = w_sem / (k + (rank + 1) as f64);
        acc.entry(file.path.clone())
            .and_modify(|(existing, s)| {
                *s += rrf;
                merge_chunk_hits(&mut existing.chunks, &file.chunks);
            })
            .or_insert_with(|| (file.clone(), rrf));
    }

    let mut merged: Vec<_> = acc.into_values().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(limit);

    merged
        .into_iter()
        .map(|(mut file, rrf_score)| {
            file.score = rrf_score;
            file
        })
        .collect()
}

/// Merge `incoming` into `existing`, deduplicating by `(line_start, line_end)`.
///
/// When a chunk exists in both lists, the one from `existing` is kept (so FTS
/// scores win over vector scores on the same chunk, which matches the order
/// `merge_rrf_files` calls this).
fn merge_chunk_hits(existing: &mut Vec<ChunkHit>, incoming: &[ChunkHit]) {
    use std::collections::HashSet;
    let seen: HashSet<(usize, usize)> = existing
        .iter()
        .map(|c| (c.line_start, c.line_end))
        .collect();
    for c in incoming {
        if !seen.contains(&(c.line_start, c.line_end)) {
            existing.push(c.clone());
        }
    }
}

/// Default hybrid search implementation used by [`RetrieveStore::search_hybrid`].
///
/// Calls `search_fts` and, when an embedder is provided, `search_similar`;
/// then merges results via [`merge_rrf_files`].  When `q.embedder` is `None`,
/// falls back to FTS-only output.
pub fn default_hybrid<S: RetrieveStore + ?Sized>(
    store: &S,
    q: &HybridQuery<'_>,
) -> Result<Vec<FileSearchResult>> {
    let over_fetch = q.limit * 3;
    let fts = store.search_fts(&FtsQuery {
        query: q.query,
        limit: over_fetch,
        path_prefix: q.path_prefix,
    })?;

    let Some(embedder) = q.embedder else {
        return Ok(fts.into_iter().take(q.limit).collect());
    };

    let sem = store.search_similar(&VectorQuery {
        query: q.query,
        embedder,
        limit: over_fetch,
        path_prefix: q.path_prefix,
    })?;

    Ok(merge_rrf_files(
        &fts,
        &sem,
        q.rrf_k,
        q.weight_fts,
        q.weight_sem,
        q.limit,
    ))
}

// ── backend factory functions ─────────────────────────────────────────────────

/// Open or create an in-memory backend.
pub fn open_in_memory() -> Arc<dyn RetrieveStore + Send + Sync> {
    Arc::new(InMemoryStore::new())
}

#[cfg(feature = "sqlite-store")]
pub fn open_sqlite_fts(db_path: &Path) -> Arc<dyn RetrieveStore + Send + Sync> {
    Arc::new(SqliteStore::new_fts_only(db_path.to_owned()))
}

#[cfg(feature = "sqlite-store")]
pub fn open_sqlite_vec(db_path: &Path, dim: u32) -> Result<Arc<dyn RetrieveStore + Send + Sync>> {
    Ok(Arc::new(SqliteStore::new_with_vec(
        db_path.to_owned(),
        dim,
    )?))
}

#[cfg(feature = "lancedb-store")]
pub fn open_lancedb(data_dir: &Path, dim: u32) -> Result<Arc<dyn RetrieveStore + Send + Sync>> {
    Ok(Arc::new(LanceDbBackend::new(data_dir, dim)?))
}
