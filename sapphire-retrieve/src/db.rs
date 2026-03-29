//! Unified retrieve database: FTS5 + vector search in a single SQLite file.
//!
//! [`RetrieveDb`] manages its own SQLite database that is separate from any
//! application cache.  It stores:
//!
//! - `documents` — the full text corpus (id, title, body, path).
//! - `documents_fts` — FTS5 trigram index over title + body.
//! - `chunks` — paragraph-level chunks derived from each document body.
//! - `chunk_vectors` (optional) — vec0 virtual table for approximate similarity
//!   search via the sqlite-vec extension.
//!
//! # Vector backends
//!
//! Call [`RetrieveDb::init_sqlite_vec`] or [`RetrieveDb::init_lancedb`] after
//! opening to enable vector search.  Without a vector backend, only FTS is
//! available.
//!
//! # Thread safety
//!
//! [`RetrieveDb`] is `Send + Sync`.  Each public method opens a short-lived
//! SQLite connection internally and drops it when done.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use rusqlite::{params, Connection};

use crate::{
    chunker::chunk_document,
    embed::Embedder,
    error::{Error, Result},
    vector_store::{Chunk, ChunkSearchResult, VecInfo, vec_serialize},
};

// ── schema ────────────────────────────────────────────────────────────────────

/// Stored in `PRAGMA user_version` of the retrieve DB.
pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS documents (
    id    INTEGER PRIMARY KEY,
    title TEXT    NOT NULL DEFAULT '',
    body  TEXT    NOT NULL DEFAULT '',
    path  TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_documents_title ON documents(title);

CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    title,
    body,
    content       = 'documents',
    content_rowid = 'id',
    tokenize      = 'trigram'
);

CREATE TABLE IF NOT EXISTS chunks (
    id          INTEGER PRIMARY KEY,
    doc_id      INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    text        TEXT    NOT NULL,
    UNIQUE (doc_id, chunk_index)
);
CREATE INDEX IF NOT EXISTS idx_chunks_doc_id ON chunks(doc_id);
";

// ── sqlite-vec extension ──────────────────────────────────────────────────────

static SQLITE_VEC_INIT: std::sync::Once = std::sync::Once::new();

fn init_sqlite_vec_extension() {
    SQLITE_VEC_INIT.call_once(|| {
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

// ── vector backend ────────────────────────────────────────────────────────────

enum VecBackend {
    SqliteVec { dim: u32 },
    #[cfg(feature = "lancedb-store")]
    LanceDb(Arc<Mutex<crate::lancedb_store::LanceDbVectorStore>>),
}

// ── public types ──────────────────────────────────────────────────────────────

/// A document to be indexed for FTS and/or vector search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Document {
    /// Stable identifier assigned by the caller.
    ///
    /// For sapphire-journal, this is the `CarettaId` stored as `i64`.  For other
    /// applications, a hash of the file path works well.
    pub id: i64,
    /// Human-readable title (shown in search results).
    pub title: String,
    /// Full body text (indexed by FTS and chunked for vector embedding).
    pub body: String,
    /// Absolute file path (shown in search results).
    pub path: String,
}

/// A search result from [`RetrieveDb::search_fts`] or a deduplicated result
/// from [`RetrieveDb::dedup_chunk_results`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub id: i64,
    pub title: String,
    pub path: String,
    /// BM25 rank (FTS, negative: more-negative = more relevant) or
    /// L2 distance (vector, lower = more similar).
    pub score: f64,
}

// ── RetrieveDb ────────────────────────────────────────────────────────────────

/// Manages a SQLite database for full-text and (optionally) vector search.
///
/// Create with [`RetrieveDb::open`], then call:
/// - [`upsert_document`](Self::upsert_document) / [`remove_document`](Self::remove_document) to keep the index in sync.
/// - [`rebuild_fts`](Self::rebuild_fts) after a batch of upserts/removes.
/// - [`search_fts`](Self::search_fts) for trigram full-text search.
/// - Optionally call [`init_sqlite_vec`](Self::init_sqlite_vec) or
///   [`init_lancedb`](Self::init_lancedb), then use
///   [`embed_pending`](Self::embed_pending) and
///   [`search_similar`](Self::search_similar) for vector search.
pub struct RetrieveDb {
    db_path: PathBuf,
    vec_backend: Mutex<Option<VecBackend>>,
}

impl RetrieveDb {
    /// Open (or create) the retrieve database at `db_path`.
    ///
    /// FTS is always available after this call.  To enable vector search,
    /// call [`init_sqlite_vec`](Self::init_sqlite_vec) or
    /// [`init_lancedb`](Self::init_lancedb) afterwards.
    pub fn open(db_path: &Path) -> Result<Self> {
        std::fs::create_dir_all(db_path.parent().unwrap_or(Path::new(".")))?;
        let conn = open_or_init(db_path)?;
        drop(conn);
        Ok(Self { db_path: db_path.to_owned(), vec_backend: Mutex::new(None) })
    }

    /// Delete the existing database and create a fresh one.
    pub fn rebuild(db_path: &Path) -> Result<Self> {
        wipe_db_files(db_path);
        Self::open(db_path)
    }

    // ── vector backend init ───────────────────────────────────────────────────

    /// Enable the sqlite-vec vector backend with `embedding_dim` dimensions.
    ///
    /// Idempotent — if the backend is already initialised this is a no-op.
    /// Returns an error only if the dimension changes (requires a rebuild).
    pub fn init_sqlite_vec(&self, embedding_dim: u32) -> Result<()> {
        let mut guard = self.vec_backend.lock().unwrap();
        if guard.is_none() {
            init_sqlite_vec_extension();
            let conn = open_or_init(&self.db_path)?;
            ensure_vec_tables(&conn, embedding_dim)?;
            *guard = Some(VecBackend::SqliteVec { dim: embedding_dim });
        }
        Ok(())
    }

    /// Enable the LanceDB vector backend.
    ///
    /// `lancedb_dir` is the directory in which LanceDB stores its data.
    /// Idempotent — if the backend is already initialised this is a no-op.
    #[cfg(feature = "lancedb-store")]
    pub fn init_lancedb(&self, lancedb_dir: &Path, embedding_dim: u32) -> Result<()> {
        let mut guard = self.vec_backend.lock().unwrap();
        if guard.is_none() {
            let store = crate::lancedb_store::LanceDbVectorStore::new(lancedb_dir, embedding_dim)?;
            *guard = Some(VecBackend::LanceDb(Arc::new(Mutex::new(store))));
        }
        Ok(())
    }

    // ── document management ───────────────────────────────────────────────────

    /// Insert or replace a document in the retrieve database.
    ///
    /// Also re-chunks the document body and updates the `chunks` table.
    /// Stale embeddings for changed chunks are removed automatically when the
    /// sqlite-vec backend is active.
    ///
    /// Call [`rebuild_fts`](Self::rebuild_fts) after a batch of upserts.
    pub fn upsert_document(&self, doc: &Document) -> Result<()> {
        let conn = self.open_conn()?;

        conn.execute(
            "INSERT OR REPLACE INTO documents (id, title, body, path) \
             VALUES (?1, ?2, ?3, ?4)",
            params![doc.id, doc.title, doc.body, doc.path],
        )?;

        let has_vec = matches!(*self.vec_backend.lock().unwrap(), Some(VecBackend::SqliteVec { .. }));
        upsert_chunks(&conn, doc, has_vec)?;
        Ok(())
    }

    /// Remove a document (and its chunks / embeddings) from the database.
    ///
    /// Performs an incremental FTS5 delete so a full rebuild is not needed
    /// for single removals.
    pub fn remove_document(&self, id: i64) -> Result<()> {
        let conn = self.open_conn()?;

        let fts_data: Option<(String, String)> = conn
            .query_row(
                "SELECT title, body FROM documents WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        let has_vec = matches!(*self.vec_backend.lock().unwrap(), Some(VecBackend::SqliteVec { .. }));
        if has_vec {
            conn.execute(
                "DELETE FROM chunk_vectors WHERE chunk_id IN \
                 (SELECT id FROM chunks WHERE doc_id = ?1)",
                [id],
            )?;
        }

        conn.execute("DELETE FROM documents WHERE id = ?1", [id])?;

        if let Some((title, body)) = fts_data {
            let _ = conn.execute(
                "INSERT INTO documents_fts(documents_fts, rowid, title, body) \
                 VALUES('delete', ?1, ?2, ?3)",
                params![id, title, body],
            );
        }

        Ok(())
    }

    /// Rebuild the FTS5 index from the current `documents` table.
    ///
    /// Call this after a batch of [`upsert_document`](Self::upsert_document)
    /// calls or whenever the FTS index may be out of date.
    pub fn rebuild_fts(&self) -> Result<()> {
        let conn = self.open_conn()?;
        conn.execute_batch("INSERT INTO documents_fts(documents_fts) VALUES('rebuild')")?;
        Ok(())
    }

    // ── search ────────────────────────────────────────────────────────────────

    /// Full-text search using the FTS5 trigram index.
    ///
    /// Returns up to `limit` documents matching `query`, ordered by relevance.
    /// The trigram tokenizer supports substring and CJK queries.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let conn = self.open_conn()?;
        let mut stmt = conn.prepare(
            "SELECT d.id, d.title, d.path, fts.rank
             FROM documents_fts fts
             JOIN documents d ON d.id = fts.rowid
             WHERE documents_fts MATCH ?1
             ORDER BY fts.rank
             LIMIT ?2",
        )?;
        let results = stmt
            .query_map(params![query, limit as i64], |row| {
                Ok(SearchResult {
                    id: row.get::<_, i64>(0)?,
                    title: row.get::<_, String>(1)?,
                    path: row.get::<_, String>(2)?,
                    score: row.get::<_, f64>(3).unwrap_or(0.0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(results)
    }

    /// Find the `limit` most similar chunks to `query_vec`.
    ///
    /// Returns an empty `Vec` when no vector backend is configured.
    pub fn search_similar(
        &self,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<ChunkSearchResult>> {
        let guard = self.vec_backend.lock().unwrap();
        match &*guard {
            None => Ok(Vec::new()),
            Some(VecBackend::SqliteVec { .. }) => {
                drop(guard);
                let conn = self.open_conn()?;
                let blob = vec_serialize(query_vec);
                let mut stmt = conn.prepare(
                    "SELECT d.id, d.title, d.path, c.chunk_index, c.text, cv.distance
                     FROM chunk_vectors cv
                     JOIN chunks c ON c.id = cv.chunk_id
                     JOIN documents d ON d.id = c.doc_id
                     WHERE cv.embedding MATCH ?1 AND k = ?2
                     ORDER BY cv.distance",
                )?;
                let results = stmt
                    .query_map(params![blob, limit as i64], |row| {
                        Ok(ChunkSearchResult {
                            doc_id: row.get::<_, i64>(0)?,
                            doc_title: row.get::<_, String>(1)?,
                            doc_path: row.get::<_, String>(2)?,
                            chunk_index: row.get::<_, i64>(3)? as usize,
                            chunk_text: row.get::<_, String>(4)?,
                            score: row.get::<_, f64>(5).unwrap_or(0.0),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(results)
            }
            #[cfg(feature = "lancedb-store")]
            Some(VecBackend::LanceDb(store_arc)) => {
                let store_arc = Arc::clone(store_arc);
                drop(guard);
                let store = store_arc.lock().unwrap();
                crate::vector_store::VectorStore::search_similar(&*store, query_vec, limit)
            }
        }
    }

    /// Deduplicate chunk search results to one `SearchResult` per document.
    ///
    /// When multiple chunks from the same document match, only the best-scoring
    /// (lowest L2 distance) chunk is kept.  Returns up to `limit` results
    /// ordered by ascending score.
    pub fn dedup_chunk_results(
        results: Vec<ChunkSearchResult>,
        limit: usize,
    ) -> Vec<SearchResult> {
        use std::collections::HashMap;

        let mut best: HashMap<i64, ChunkSearchResult> = HashMap::new();
        for r in results {
            best.entry(r.doc_id)
                .and_modify(|e| {
                    if r.score < e.score {
                        *e = r.clone();
                    }
                })
                .or_insert(r);
        }

        let mut deduped: Vec<_> = best.into_values().collect();
        deduped.sort_by(|a, b| {
            a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal)
        });
        deduped.truncate(limit);
        deduped
            .into_iter()
            .map(|r| SearchResult {
                id: r.doc_id,
                title: r.doc_title,
                path: r.doc_path,
                score: r.score,
            })
            .collect()
    }

    // ── embedding ─────────────────────────────────────────────────────────────

    /// Generate and store embeddings for all pending (unembedded) chunks.
    ///
    /// Returns the number of newly embedded chunks.  Returns 0 immediately
    /// when no vector backend is configured.
    ///
    /// `on_progress(done, total)` is called after each batch of 100 chunks.
    pub fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        let guard = self.vec_backend.lock().unwrap();
        match &*guard {
            None => Ok(0),
            Some(VecBackend::SqliteVec { .. }) => {
                drop(guard);
                self.embed_pending_sqlite_vec(embedder, on_progress)
            }
            #[cfg(feature = "lancedb-store")]
            Some(VecBackend::LanceDb(store_arc)) => {
                let store_arc = Arc::clone(store_arc);
                drop(guard);
                let store = store_arc.lock().unwrap();
                self.embed_pending_lancedb(&*store, embedder, on_progress)
            }
        }
    }

    /// Read vector index statistics.
    pub fn vec_info(&self) -> Result<VecInfo> {
        let guard = self.vec_backend.lock().unwrap();
        match &*guard {
            None => Ok(VecInfo { embedding_dim: 0, vector_count: 0, pending_count: 0 }),
            Some(VecBackend::SqliteVec { dim }) => {
                let dim = *dim;
                drop(guard);
                let conn = self.open_conn()?;
                let vector_count: u64 = conn
                    .query_row("SELECT COUNT(*) FROM chunk_vectors", [], |row| row.get(0))
                    .unwrap_or(0);
                let chunk_count: u64 = conn
                    .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
                    .unwrap_or(0);
                Ok(VecInfo {
                    embedding_dim: dim,
                    vector_count,
                    pending_count: chunk_count.saturating_sub(vector_count),
                })
            }
            #[cfg(feature = "lancedb-store")]
            Some(VecBackend::LanceDb(store_arc)) => {
                let store_arc = Arc::clone(store_arc);
                drop(guard);
                let conn = self.open_conn()?;
                let store = store_arc.lock().unwrap();
                store.vec_info(&conn)
            }
        }
    }

    /// Return the IDs of all documents in the database.
    pub fn document_ids(&self) -> Result<Vec<i64>> {
        let conn = self.open_conn()?;
        let mut stmt = conn.prepare("SELECT id FROM documents")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Return the total number of documents in the database.
    pub fn document_count(&self) -> Result<u64> {
        let conn = self.open_conn()?;
        let count: u64 =
            conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;
        Ok(count)
    }

    // ── private helpers ───────────────────────────────────────────────────────

    fn open_conn(&self) -> Result<Connection> {
        // If sqlite-vec is active, ensure the extension is registered globally
        // (a process-global once-only registration is sufficient).
        if matches!(*self.vec_backend.lock().unwrap(), Some(VecBackend::SqliteVec { .. })) {
            init_sqlite_vec_extension();
        }
        let conn = Connection::open(&self.db_path)?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        Ok(conn)
    }

    fn embed_pending_sqlite_vec(
        &self,
        embedder: &dyn Embedder,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        let conn = self.open_conn()?;
        let embedded_keys = sqlite_vec_embedded_keys(&conn)?;
        let pending = collect_pending_chunks(&conn, &embedded_keys)?;
        let total = pending.len();
        let mut done = 0;

        for batch in pending.chunks(100) {
            let texts: Vec<&str> = batch.iter().map(|c| c.text.as_str()).collect();
            let embeddings = embedder.embed_texts(&texts)?;
            sqlite_vec_insert_embeddings(&conn, batch, &embeddings)?;
            done += batch.len();
            on_progress(done, total);
        }
        Ok(total)
    }

    #[cfg(feature = "lancedb-store")]
    fn embed_pending_lancedb(
        &self,
        store: &crate::lancedb_store::LanceDbVectorStore,
        embedder: &dyn Embedder,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        use crate::vector_store::VectorStore as _;

        let conn = self.open_conn()?;
        let embedded_keys = store.embedded_chunk_keys()?;
        let pending = collect_pending_chunks(&conn, &embedded_keys)?;
        let total = pending.len();
        let mut done = 0;

        for batch in pending.chunks(100) {
            let texts: Vec<&str> = batch.iter().map(|c| c.text.as_str()).collect();
            let embeddings = embedder.embed_texts(&texts)?;
            store.insert_embeddings(batch, &embeddings)?;
            done += batch.len();
            on_progress(done, total);
        }
        Ok(total)
    }
}

// ── open / init helpers ───────────────────────────────────────────────────────

fn open_or_init(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

    let db_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if db_version == 0 {
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        return Ok(conn);
    }

    if db_version == SCHEMA_VERSION {
        return Ok(conn);
    }

    Err(Error::SchemaTooNew {
        db_version,
        app_version: SCHEMA_VERSION,
    })
}

fn wipe_db_files(db_path: &Path) {
    let base = db_path.to_string_lossy();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{base}{suffix}"));
    }
}

fn ensure_vec_tables(conn: &Connection, dim: u32) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vec_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )?;

    let stored_dim: Option<u32> = conn
        .query_row(
            "SELECT value FROM vec_meta WHERE key = 'embedding_dim'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok());

    match stored_dim {
        None => {
            conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE chunk_vectors USING vec0(\
                 chunk_id INTEGER PRIMARY KEY, embedding FLOAT[{dim}])"
            ))?;
            conn.execute(
                "INSERT OR REPLACE INTO vec_meta (key, value) VALUES ('embedding_dim', ?1)",
                [dim.to_string()],
            )?;
        }
        Some(d) if d == dim => {
            conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vectors USING vec0(\
                 chunk_id INTEGER PRIMARY KEY, embedding FLOAT[{dim}])"
            ))?;
        }
        Some(old) => {
            eprintln!(
                "info: embedding dimension changed ({old} → {dim}), \
                 recreating vector table (all stored embeddings will be lost)..."
            );
            conn.execute_batch("DROP TABLE IF EXISTS chunk_vectors")?;
            conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE chunk_vectors USING vec0(\
                 chunk_id INTEGER PRIMARY KEY, embedding FLOAT[{dim}])"
            ))?;
            conn.execute(
                "INSERT OR REPLACE INTO vec_meta (key, value) VALUES ('embedding_dim', ?1)",
                [dim.to_string()],
            )?;
        }
    }
    Ok(())
}

// ── chunk helpers ─────────────────────────────────────────────────────────────

fn upsert_chunks(conn: &Connection, doc: &Document, has_vec: bool) -> Result<()> {
    let chunk_texts = chunk_document(&doc.title, &doc.body);

    // Delete excess chunks (handles body that shrank).
    if has_vec {
        conn.execute(
            "DELETE FROM chunk_vectors WHERE chunk_id IN \
             (SELECT id FROM chunks WHERE doc_id = ?1 AND chunk_index >= ?2)",
            params![doc.id, chunk_texts.len() as i64],
        )?;
    }
    conn.execute(
        "DELETE FROM chunks WHERE doc_id = ?1 AND chunk_index >= ?2",
        params![doc.id, chunk_texts.len() as i64],
    )?;

    for (idx, text) in chunk_texts.iter().enumerate() {
        conn.execute(
            "INSERT INTO chunks (doc_id, chunk_index, text) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_id, chunk_index) DO UPDATE
             SET text = excluded.text
             WHERE text != excluded.text",
            params![doc.id, idx as i64, text],
        )?;
        // Row inserted or text changed → stale embedding no longer valid.
        if has_vec && conn.changes() > 0 {
            conn.execute(
                "DELETE FROM chunk_vectors WHERE chunk_id = \
                 (SELECT id FROM chunks WHERE doc_id = ?1 AND chunk_index = ?2)",
                params![doc.id, idx as i64],
            )?;
        }
    }
    Ok(())
}

// ── sqlite-vec query helpers ──────────────────────────────────────────────────

fn sqlite_vec_embedded_keys(conn: &Connection) -> Result<HashSet<(i64, usize)>> {
    let mut stmt = conn.prepare(
        "SELECT c.doc_id, c.chunk_index
         FROM chunks c
         JOIN chunk_vectors cv ON cv.chunk_id = c.id",
    )?;
    let keys = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as usize))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(keys)
}

fn collect_pending_chunks(
    conn: &Connection,
    embedded_keys: &HashSet<(i64, usize)>,
) -> Result<Vec<Chunk>> {
    let mut stmt = conn.prepare(
        "SELECT c.doc_id, c.chunk_index, c.text, d.title, d.path
         FROM chunks c
         JOIN documents d ON d.id = c.doc_id",
    )?;
    let chunks = stmt
        .query_map([], |row| {
            Ok(Chunk {
                doc_id: row.get::<_, i64>(0)?,
                chunk_index: row.get::<_, i64>(1)? as usize,
                text: row.get::<_, String>(2)?,
                doc_title: row.get::<_, String>(3)?,
                doc_path: row.get::<_, String>(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .filter(|c| !embedded_keys.contains(&(c.doc_id, c.chunk_index)))
        .collect();
    Ok(chunks)
}

fn sqlite_vec_insert_embeddings(
    conn: &Connection,
    chunks: &[Chunk],
    embeddings: &[Vec<f32>],
) -> Result<()> {
    for (chunk, emb) in chunks.iter().zip(embeddings) {
        let chunk_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM chunks WHERE doc_id = ?1 AND chunk_index = ?2",
                params![chunk.doc_id, chunk.chunk_index as i64],
                |row| row.get(0),
            )
            .ok();

        if let Some(id) = chunk_id {
            let blob = vec_serialize(emb);
            conn.execute(
                "INSERT OR REPLACE INTO chunk_vectors (chunk_id, embedding) VALUES (?1, ?2)",
                params![id, blob],
            )?;
        }
    }
    Ok(())
}
