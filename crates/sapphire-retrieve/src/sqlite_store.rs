//! SQLite backend for [`RetrieveStore`].
//!
//! [`SqliteStore`] stores all data in a single SQLite file using:
//!
//! - FTS5 trigram index for full-text search (substring / CJK aware).
//! - `sqlite-vec` virtual table for approximate nearest-neighbour search.
//!
//! The sqlite-vec extension is optional: construct with
//! [`SqliteStore::new_fts_only`] to get FTS without vector search, or
//! [`SqliteStore::new_with_vec`] to enable both.
//!
//! # Schema
//!
//! | table | purpose |
//! |-------|---------|
//! | `files` | path + mtime tracking |
//! | `documents` | full text corpus |
//! | `documents_fts` | FTS5 trigram index |
//! | `chunks` | paragraph-level chunks |
//! | `chunk_vectors` | sqlite-vec virtual table (optional) |
//!
//! The SQLite schema version is stored in `PRAGMA user_version`.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Once,
};

use rusqlite::{Connection, params};

use crate::{
    chunker::chunk_document,
    embed::Embedder,
    error::{Error, Result},
    retrieve_store::{Document, FtsQuery, RetrieveStore, SearchResult, VectorQuery},
    vector_store::{Chunk, ChunkSearchResult, VecInfo, vec_serialize},
};

// ── schema ────────────────────────────────────────────────────────────────────

/// Stored in `PRAGMA user_version` of the SQLite retrieve DB.
///
/// Version history:
/// - 1: initial schema
/// - 2: sqlite-vec integration
/// - 3: replace `chunk_index` with `line` + `column` (source positions)
/// - 4: add index on `documents(path)` for path-prefix filtering
pub const SCHEMA_VERSION: i32 = 4;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path       TEXT    PRIMARY KEY,
    file_mtime INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS documents (
    id    INTEGER PRIMARY KEY,
    title TEXT    NOT NULL DEFAULT '',
    body  TEXT    NOT NULL DEFAULT '',
    path  TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_documents_title ON documents(title);
CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(path);

CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    title,
    body,
    content       = 'documents',
    content_rowid = 'id',
    tokenize      = 'trigram'
);

CREATE TABLE IF NOT EXISTS chunks (
    id     INTEGER PRIMARY KEY,
    doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    line   INTEGER NOT NULL,
    col    INTEGER NOT NULL DEFAULT 0,
    text   TEXT    NOT NULL,
    UNIQUE (doc_id, line)
);
CREATE INDEX IF NOT EXISTS idx_chunks_doc_id ON chunks(doc_id);
";

// ── sqlite-vec extension init ─────────────────────────────────────────────────

static SQLITE_VEC_INIT: Once = Once::new();

fn init_sqlite_vec_extension() {
    SQLITE_VEC_INIT.call_once(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

// ── SqliteStore ───────────────────────────────────────────────────────────────

/// SQLite-backed retrieve store.
///
/// Manages FTS5 full-text search and, optionally, sqlite-vec vector search.
///
/// Construct via [`SqliteStore::new_fts_only`] (FTS only) or
/// [`SqliteStore::new_with_vec`] (FTS + vector search).
pub struct SqliteStore {
    db_path: PathBuf,
    /// `None` = FTS only; `Some(dim)` = FTS + sqlite-vec with `dim`-dimensional
    /// embeddings.
    dim: Option<u32>,
}

impl SqliteStore {
    /// Create a FTS-only store (no vector search).
    ///
    /// The SQLite file is created lazily on first use.
    pub fn new_fts_only(db_path: PathBuf) -> Self {
        Self { db_path, dim: None }
    }

    /// Create a store with both FTS and sqlite-vec vector search enabled.
    ///
    /// Initialises the vector tables immediately.
    pub fn new_with_vec(db_path: PathBuf, embedding_dim: u32) -> Result<Self> {
        init_sqlite_vec_extension();
        let conn = open_or_init(&db_path)?;
        ensure_vec_tables(&conn, embedding_dim)?;
        Ok(Self {
            db_path,
            dim: Some(embedding_dim),
        })
    }

    /// Return the vector embedding dimension, or `None` if vector search is
    /// not enabled.
    pub fn dim(&self) -> Option<u32> {
        self.dim
    }

    fn open_conn(&self) -> Result<Connection> {
        if self.dim.is_some() {
            init_sqlite_vec_extension();
        }
        open_or_init(&self.db_path)
    }
}

impl RetrieveStore for SqliteStore {
    // ── file tracking ──────────────────────────────────────────────────────────

    fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        let conn = self.open_conn()?;
        let mut stmt = conn.prepare("SELECT path, file_mtime FROM files")?;
        let result = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(result)
    }

    fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        let conn = self.open_conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO files (path, file_mtime) VALUES (?1, ?2)",
            params![path, mtime],
        )?;
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        let conn = self.open_conn()?;
        conn.execute("DELETE FROM files WHERE path = ?1", [path])?;
        Ok(())
    }

    fn file_count(&self) -> Result<u64> {
        let conn = self.open_conn()?;
        let count: u64 =
            conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))? as u64;
        Ok(count)
    }

    // ── document management ────────────────────────────────────────────────────

    fn upsert_document(&self, doc: &Document) -> Result<()> {
        let conn = self.open_conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO documents (id, title, body, path) \
             VALUES (?1, ?2, ?3, ?4)",
            params![doc.id, doc.title, doc.body, doc.path],
        )?;
        upsert_chunks(&conn, doc, self.dim.is_some())?;
        Ok(())
    }

    fn remove_document(&self, id: i64) -> Result<()> {
        let conn = self.open_conn()?;

        let fts_data: Option<(String, String)> = conn
            .query_row(
                "SELECT title, body FROM documents WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        if self.dim.is_some() {
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

    fn rebuild_fts(&self) -> Result<()> {
        let conn = self.open_conn()?;
        conn.execute_batch("INSERT INTO documents_fts(documents_fts) VALUES('rebuild')")?;
        Ok(())
    }

    fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<SearchResult>> {
        let conn = self.open_conn()?;
        let prefix_glob = q.path_prefix.map(|p| format!("{}*", p.to_string_lossy()));
        let sql = if prefix_glob.is_some() {
            "SELECT d.id, d.title, d.path, fts.rank
             FROM documents_fts fts
             JOIN documents d ON d.id = fts.rowid
             WHERE documents_fts MATCH ?1 AND d.path GLOB ?3
             ORDER BY fts.rank
             LIMIT ?2"
        } else {
            "SELECT d.id, d.title, d.path, fts.rank
             FROM documents_fts fts
             JOIN documents d ON d.id = fts.rowid
             WHERE documents_fts MATCH ?1
             ORDER BY fts.rank
             LIMIT ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let results = if let Some(ref glob) = prefix_glob {
            stmt.query_map(params![q.query, q.limit as i64, glob], |row| {
                Ok(SearchResult {
                    id: row.get::<_, i64>(0)?,
                    title: row.get::<_, String>(1)?,
                    path: row.get::<_, String>(2)?,
                    score: row.get::<_, f64>(3).unwrap_or(0.0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![q.query, q.limit as i64], |row| {
                Ok(SearchResult {
                    id: row.get::<_, i64>(0)?,
                    title: row.get::<_, String>(1)?,
                    path: row.get::<_, String>(2)?,
                    score: row.get::<_, f64>(3).unwrap_or(0.0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(results)
    }

    fn document_ids(&self) -> Result<Vec<i64>> {
        let conn = self.open_conn()?;
        let mut stmt = conn.prepare("SELECT id FROM documents")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    fn document_count(&self) -> Result<u64> {
        let conn = self.open_conn()?;
        let count: u64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |row| {
            row.get::<_, i64>(0)
        })? as u64;
        Ok(count)
    }

    // ── vector / embedding ─────────────────────────────────────────────────────

    fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<usize> {
        if self.dim.is_none() {
            return Ok(0);
        }
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

    fn vec_info(&self) -> Result<VecInfo> {
        let Some(dim) = self.dim else {
            return Ok(VecInfo {
                embedding_dim: 0,
                vector_count: 0,
                pending_count: 0,
            });
        };
        let conn = self.open_conn()?;
        let vector_count: u64 = conn
            .query_row("SELECT COUNT(*) FROM chunk_vectors", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0) as u64;
        let chunk_count: u64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0) as u64;
        Ok(VecInfo {
            embedding_dim: dim,
            vector_count,
            pending_count: chunk_count.saturating_sub(vector_count),
        })
    }

    fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<ChunkSearchResult>> {
        if self.dim.is_none() {
            return Ok(Vec::new());
        }
        let conn = self.open_conn()?;
        let blob = vec_serialize(q.query_vec);

        // sqlite-vec doesn't support WHERE filters on the virtual table join
        // directly, so we over-fetch and post-filter when a path prefix is given.
        let over_fetch = if q.path_prefix.is_some() {
            q.limit * 5
        } else {
            q.limit
        };

        let mut stmt = conn.prepare(
            "SELECT d.id, d.title, d.path, c.line, c.col, c.text, cv.distance
             FROM chunk_vectors cv
             JOIN chunks c ON c.id = cv.chunk_id
             JOIN documents d ON d.id = c.doc_id
             WHERE cv.embedding MATCH ?1 AND k = ?2
             ORDER BY cv.distance",
        )?;
        let prefix = q.path_prefix.map(|p| p.to_string_lossy().to_string());
        let results: Vec<ChunkSearchResult> = stmt
            .query_map(params![blob, over_fetch as i64], |row| {
                Ok(ChunkSearchResult {
                    doc_id: row.get::<_, i64>(0)?,
                    doc_title: row.get::<_, String>(1)?,
                    doc_path: row.get::<_, String>(2)?,
                    line: row.get::<_, i64>(3)? as usize,
                    column: row.get::<_, i64>(4)? as usize,
                    chunk_text: row.get::<_, String>(5)?,
                    score: row.get::<_, f64>(6).unwrap_or(0.0),
                })
            })?
            .filter_map(|r| r.ok())
            .filter(|r| {
                prefix
                    .as_ref()
                    .map_or(true, |pfx| r.doc_path.starts_with(pfx.as_str()))
            })
            .take(q.limit)
            .collect();
        Ok(results)
    }
}

// ── open / init helpers ───────────────────────────────────────────────────────

pub(crate) fn open_or_init(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
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

    if db_version < SCHEMA_VERSION {
        apply_migrations(&conn, db_version)?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        return Ok(conn);
    }

    Err(Error::SchemaTooNew {
        db_version,
        app_version: SCHEMA_VERSION,
    })
}

fn apply_migrations(conn: &Connection, from_version: i32) -> Result<()> {
    if from_version < 4 {
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(path)")?;
    }
    Ok(())
}

pub fn wipe_db_files(db_path: &Path) {
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
    // Build the list of (line, col, embed_text) tuples.
    //
    // When the caller provides pre-computed chunks (e.g. from JsonChunker),
    // `line` is the 0-based source line number and `col` is the byte column.
    // Otherwise we fall back to chunk_document() with sequential line=0,1,2,…
    // and col=0.
    let computed: Vec<(usize, usize, String)>;
    let chunks: &[(usize, usize, String)] = if let Some(ref c) = doc.chunks {
        c.as_slice()
    } else {
        computed = chunk_document(&doc.title, &doc.body)
            .into_iter()
            .enumerate()
            .map(|(i, t)| (i, 0usize, t))
            .collect();
        &computed
    };

    let live_lines: std::collections::HashSet<i64> =
        chunks.iter().map(|(line, _, _)| *line as i64).collect();

    // Delete stale chunks (those no longer present in the new set).
    let old_lines: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT line FROM chunks WHERE doc_id = ?1")?;
        stmt.query_map([doc.id], |row| row.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .filter(|l| !live_lines.contains(l))
            .collect()
    };
    for line in old_lines {
        if has_vec {
            conn.execute(
                "DELETE FROM chunk_vectors WHERE chunk_id = \
                 (SELECT id FROM chunks WHERE doc_id = ?1 AND line = ?2)",
                params![doc.id, line],
            )?;
        }
        conn.execute(
            "DELETE FROM chunks WHERE doc_id = ?1 AND line = ?2",
            params![doc.id, line],
        )?;
    }

    // Upsert each chunk; if text changed, invalidate the stale embedding.
    for (line, col, text) in chunks {
        conn.execute(
            "INSERT INTO chunks (doc_id, line, col, text) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(doc_id, line) DO UPDATE
             SET col = excluded.col, text = excluded.text
             WHERE text != excluded.text",
            params![doc.id, *line as i64, *col as i64, text],
        )?;
        if has_vec && conn.changes() > 0 {
            conn.execute(
                "DELETE FROM chunk_vectors WHERE chunk_id = \
                 (SELECT id FROM chunks WHERE doc_id = ?1 AND line = ?2)",
                params![doc.id, *line as i64],
            )?;
        }
    }
    Ok(())
}

// ── sqlite-vec query helpers ──────────────────────────────────────────────────

fn sqlite_vec_embedded_keys(conn: &Connection) -> Result<HashSet<(i64, usize)>> {
    let mut stmt = conn.prepare(
        "SELECT c.doc_id, c.line
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
        "SELECT c.doc_id, c.line, c.col, c.text, d.title, d.path
         FROM chunks c
         JOIN documents d ON d.id = c.doc_id",
    )?;
    let chunks = stmt
        .query_map([], |row| {
            Ok(Chunk {
                doc_id: row.get::<_, i64>(0)?,
                line: row.get::<_, i64>(1)? as usize,
                column: row.get::<_, i64>(2)? as usize,
                text: row.get::<_, String>(3)?,
                doc_title: row.get::<_, String>(4)?,
                doc_path: row.get::<_, String>(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .filter(|c| !embedded_keys.contains(&(c.doc_id, c.line)))
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
                "SELECT id FROM chunks WHERE doc_id = ?1 AND line = ?2",
                params![chunk.doc_id, chunk.line as i64],
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
