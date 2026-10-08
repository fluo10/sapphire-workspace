//! Unified retrieve database: FTS5 + vector search.
//!
//! [`RetrieveDb`] is the main entry point.  It manages one of the available
//! storage backends and exposes a unified API for document management,
//! full-text search, and vector search.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{
    embed::Embedder,
    error::Result,
    retrieve_store::{
        Document, FileSearchResult, FtsQuery, HybridQuery, RetrieveStore, VectorQuery,
    },
    snippet::leading,
    vector_store::VecInfo,
};

#[cfg(feature = "redb-store")]
use crate::redb_store::RedbStore;

/// Derive the redb store directory for a given retrieve DB file path.
///
/// Callers pass a versioned file path (e.g. `retrieve_v5.db`); the pure-Rust
/// backend stores its data in a sibling directory (`retrieve_v5.redb/`).
#[cfg(feature = "redb-store")]
fn redb_dir_for(db_path: &Path) -> PathBuf {
    db_path.with_extension("redb")
}

// ── in-memory backend ─────────────────────────────────────────────────────────

/// In-memory backend used when no persistent storage feature is compiled in.
///
/// Data lives in `HashMap`s and is lost when the process exits.
struct InMemoryStore {
    state: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
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
                if let Some(ref pfx) = prefix
                    && !doc.path.starts_with(pfx.as_str())
                {
                    return false;
                }
                doc.body.to_lowercase().contains(&needle)
            })
            .take(q.limit)
            .map(|doc| FileSearchResult {
                id: doc.id,
                path: doc.path.clone(),
                score: 0.0,
                snippet: leading(&doc.body),
            })
            .collect();
        results.sort_by(|a, b| a.path.cmp(&b.path));
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
    #[cfg(feature = "redb-store")]
    Redb(Arc<RedbStore>),
}

impl BackendState {
    fn as_store(&self) -> Arc<dyn RetrieveStore> {
        match self {
            BackendState::InMemory(s) => Arc::clone(s) as Arc<dyn RetrieveStore>,
            #[cfg(feature = "redb-store")]
            BackendState::Redb(s) => Arc::clone(s) as Arc<dyn RetrieveStore>,
        }
    }

    fn needs_init(&self) -> bool {
        match self {
            BackendState::InMemory(_) => true,
            #[cfg(feature = "redb-store")]
            BackendState::Redb(s) => s.dim().is_none(),
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
        #[cfg(feature = "redb-store")]
        {
            let store = RedbStore::open(&redb_dir_for(db_path), None)?;
            Ok(Self {
                db_path: db_path.to_owned(),
                backend: Mutex::new(BackendState::Redb(Arc::new(store))),
            })
        }

        #[cfg(not(feature = "redb-store"))]
        Ok(Self {
            db_path: db_path.to_owned(),
            backend: Mutex::new(BackendState::InMemory(Arc::new(InMemoryStore::new()))),
        })
    }

    pub fn rebuild(db_path: &Path) -> Result<Self> {
        #[cfg(feature = "redb-store")]
        crate::redb_store::wipe_store(&redb_dir_for(db_path));
        Self::open(db_path)
    }

    /// Initialise the pure-Rust redb backend with vector search enabled.
    #[cfg(feature = "redb-store")]
    pub fn init_redb_vec(&self, embedding_dim: u32) -> Result<()> {
        let mut guard = self.backend.lock().unwrap();
        if guard.needs_init() {
            let store = RedbStore::open(&redb_dir_for(&self.db_path), Some(embedding_dim))?;
            *guard = BackendState::Redb(Arc::new(store));
        }
        Ok(())
    }

    fn store(&self) -> Arc<dyn RetrieveStore> {
        self.backend.lock().unwrap().as_store()
    }

    /// バックエンドへの共有ハンドル。
    ///
    /// 同じプロセスの別コンポーネント（例: app server の別ワークスペース処理）に、
    /// このデータベースと**同じ**インデックスを使わせるためのもの。別に開くと
    /// 同じファイル群に対してインデックスが二重にできる。
    pub fn shared(&self) -> Arc<dyn RetrieveStore + Send + Sync> {
        self.store()
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
}

// ── free functions ────────────────────────────────────────────────────────────

/// Merge FTS and semantic file-level results via Reciprocal Rank Fusion.
///
/// `score(d) = w_fts / (k + rank_fts) + w_sem / (k + rank_sem)`.  Output is
/// sorted by descending RRF score.
///
/// FTS results are inserted first, so a file found by both searches keeps its
/// FTS snippet (the fragment around the match) rather than the leading text.
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
            .and_modify(|(_, s)| *s += rrf)
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

/// Open a pure-Rust redb + tantivy backend (FTS only) for the given DB file
/// path (the store is placed in a sibling `*.redb/` directory).
#[cfg(feature = "redb-store")]
pub fn open_redb(db_path: &Path) -> Result<Arc<dyn RetrieveStore + Send + Sync>> {
    Ok(Arc::new(RedbStore::open(&redb_dir_for(db_path), None)?))
}

/// Open a pure-Rust redb + tantivy backend with vector search enabled.
#[cfg(feature = "redb-store")]
pub fn open_redb_vec(db_path: &Path, dim: u32) -> Result<Arc<dyn RetrieveStore + Send + Sync>> {
    Ok(Arc::new(RedbStore::open(
        &redb_dir_for(db_path),
        Some(dim),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_store_sees_documents_written_through_the_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = RetrieveDb::open(&tmp.path().join("retrieve.db")).unwrap();

        let shared = db.shared();
        db.upsert_document(&Document {
            id: 1,
            body: "hello".to_owned(),
            path: "a.md".to_owned(),
        })
        .unwrap();

        // 同じバックエンドを指しているので、共有ハンドル側からも見える。
        assert_eq!(shared.document_count().unwrap(), 1);
    }

    #[test]
    fn hybrid_merge_keeps_the_fts_snippet_and_sums_scores() {
        let f = |id, snip: &str, score| FileSearchResult {
            id,
            path: format!("/w/{id}.md"),
            score,
            snippet: snip.into(),
        };
        let merged = merge_rrf_files(
            &[f(1, "fts", 1.0)],
            &[f(1, "lead", 0.5), f(2, "lead2", 0.7)],
            60.0,
            0.5,
            0.5,
            10,
        );
        assert_eq!(merged[0].id, 1);
        assert_eq!(merged[0].snippet, "fts");
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn in_memory_fts_returns_a_snippet() {
        let store = InMemoryStore::new();
        store
            .upsert_document(&Document {
                id: 1,
                body: "hello world".into(),
                path: "/w/a.md".into(),
            })
            .unwrap();
        let hits = store.search_fts(&FtsQuery::new("world").limit(5)).unwrap();
        assert_eq!(hits[0].snippet, "hello world");
    }
}
