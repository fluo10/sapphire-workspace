//! SQLite backend for [`RetrieveStore`].
//!
//! [`SqliteStore`] stores all data in a single SQLite file using:
//!
//! - FTS5 trigram index over the `chunks` table for full-text search.
//! - `sqlite-vec` virtual table for approximate nearest-neighbour search.
//!
//! # Schema
//!
//! | table | purpose |
//! |-------|---------|
//! | `files` | path + mtime tracking |
//! | `documents` | id / path |
//! | `chunks` | per-chunk text + source line range |
//! | `chunks_fts` | FTS5 trigram index over `chunks.text` |
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
    retrieve_store::{ChunkHit, Document, FileSearchResult, FtsQuery, RetrieveStore, VectorQuery},
    vector_store::{Chunk, VecInfo, vec_serialize},
};

// ── schema ────────────────────────────────────────────────────────────────────

/// Stored in `PRAGMA user_version` of the SQLite retrieve DB.
///
/// Version history:
/// - 1: initial schema
/// - 2: sqlite-vec integration
/// - 3: replace `chunk_index` with `line` + `column` (source positions)
/// - 4: chunk-level FTS (`chunks_fts`), `line_start`/`line_end`, drop
///   `documents.body` and `documents_fts`
pub const SCHEMA_VERSION: i32 = 4;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path       TEXT    PRIMARY KEY,
    file_mtime INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS documents (
    id    INTEGER PRIMARY KEY,
    path  TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_documents_path  ON documents(path);

CREATE TABLE IF NOT EXISTS chunks (
    id         INTEGER PRIMARY KEY,
    doc_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    line_start INTEGER NOT NULL,
    line_end   INTEGER NOT NULL,
    text       TEXT    NOT NULL,
    UNIQUE (doc_id, line_start)
);
CREATE INDEX IF NOT EXISTS idx_chunks_doc_id ON chunks(doc_id);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text,
    content       = 'chunks',
    content_rowid = 'id',
    tokenize      = 'trigram'
);
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

pub struct SqliteStore {
    db_path: PathBuf,
    dim: Option<u32>,
}

impl SqliteStore {
    pub fn new_fts_only(db_path: PathBuf) -> Self {
        Self { db_path, dim: None }
    }

    pub fn new_with_vec(db_path: PathBuf, embedding_dim: u32) -> Result<Self> {
        init_sqlite_vec_extension();
        let conn = open_or_init(&db_path)?;
        ensure_vec_tables(&conn, embedding_dim)?;
        Ok(Self {
            db_path,
            dim: Some(embedding_dim),
        })
    }

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
            "INSERT OR REPLACE INTO documents (id, path) VALUES (?1, ?2)",
            params![doc.id, doc.path],
        )?;
        upsert_chunks(&conn, doc, self.dim.is_some())?;
        Ok(())
    }

    fn remove_document(&self, id: i64) -> Result<()> {
        let conn = self.open_conn()?;

        // Capture chunk rows for incremental FTS delete before we cascade.
        let stale_chunks: Vec<(i64, String)> = {
            let mut stmt = conn.prepare("SELECT id, text FROM chunks WHERE doc_id = ?1")?;
            stmt.query_map([id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        if self.dim.is_some() {
            conn.execute(
                "DELETE FROM chunk_vectors WHERE chunk_id IN \
                 (SELECT id FROM chunks WHERE doc_id = ?1)",
                [id],
            )?;
        }

        conn.execute("DELETE FROM documents WHERE id = ?1", [id])?;

        for (cid, text) in stale_chunks {
            let _ = conn.execute(
                "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                params![cid, text],
            );
        }

        Ok(())
    }

    fn rebuild_fts(&self) -> Result<()> {
        let conn = self.open_conn()?;
        conn.execute_batch("INSERT INTO chunks_fts(chunks_fts) VALUES('rebuild')")?;
        Ok(())
    }

    fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<FileSearchResult>> {
        let conn = self.open_conn()?;
        let over_fetch = (q.limit * 5) as i64;
        let prefix_glob = q.path_prefix.map(|p| format!("{}*", p.to_string_lossy()));
        let sql = if prefix_glob.is_some() {
            "SELECT c.doc_id, d.path, c.line_start, c.line_end, c.text, fts.rank
             FROM chunks_fts fts
             JOIN chunks c    ON c.id = fts.rowid
             JOIN documents d ON d.id = c.doc_id
             WHERE chunks_fts MATCH ?1 AND d.path GLOB ?3
             ORDER BY fts.rank
             LIMIT ?2"
        } else {
            "SELECT c.doc_id, d.path, c.line_start, c.line_end, c.text, fts.rank
             FROM chunks_fts fts
             JOIN chunks c    ON c.id = fts.rowid
             JOIN documents d ON d.id = c.doc_id
             WHERE chunks_fts MATCH ?1
             ORDER BY fts.rank
             LIMIT ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<ChunkRow> = if let Some(ref glob) = prefix_glob {
            stmt.query_map(params![q.query, over_fetch, glob], map_chunk_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![q.query, over_fetch], map_chunk_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        // FTS rank is negative; more-negative = more relevant.  Lower score wins.
        Ok(group_by_file(rows, q.limit, |a, b| a < b))
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

    fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<FileSearchResult>> {
        if self.dim.is_none() {
            return Ok(Vec::new());
        }
        let conn = self.open_conn()?;

        // Embed query text.
        let query_vecs = q.embedder.embed_texts(&[q.query])?;
        let query_vec = query_vecs
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embed("embedder returned empty result".into()))?;
        let blob = vec_serialize(&query_vec);

        // Over-fetch so grouping + path-prefix filtering doesn't starve us.
        let over_fetch = (q.limit * 5) as i64;

        let mut stmt = conn.prepare(
            "SELECT d.id, d.path, c.line_start, c.line_end, c.text, cv.distance
             FROM chunk_vectors cv
             JOIN chunks c    ON c.id = cv.chunk_id
             JOIN documents d ON d.id = c.doc_id
             WHERE cv.embedding MATCH ?1 AND k = ?2
             ORDER BY cv.distance",
        )?;
        let prefix = q.path_prefix.map(|p| p.to_string_lossy().to_string());
        let rows: Vec<ChunkRow> = stmt
            .query_map(params![blob, over_fetch], map_chunk_row)?
            .filter_map(|r| r.ok())
            .filter(|r| {
                prefix
                    .as_ref()
                    .map_or(true, |pfx| r.path.starts_with(pfx.as_str()))
            })
            .collect();

        // Vector distance: lower = better.
        Ok(group_by_file(rows, q.limit, |a, b| a < b))
    }
}

// ── open / init helpers ───────────────────────────────────────────────────────

pub(crate) fn open_or_init(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Check for existing DB version first.  If it pre-dates the current
    // schema, wipe-and-recreate (SCHEMA_VERSION 4 is a hard break: old
    // `documents.body` / `documents_fts` / `chunks.line` layouts are
    // incompatible with the new chunk-level FTS design).
    let db_version: i32 = {
        let conn = Connection::open(db_path)?;
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?
    };

    if db_version != 0 && db_version < SCHEMA_VERSION {
        wipe_db_files(db_path);
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

    Err(Error::SchemaTooNew {
        db_version,
        app_version: SCHEMA_VERSION,
    })
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

// ── chunk row / grouping helpers ─────────────────────────────────────────────

struct ChunkRow {
    doc_id: i64,
    path: String,
    line_start: usize,
    line_end: usize,
    text: String,
    score: f64,
}

fn map_chunk_row(row: &rusqlite::Row) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        doc_id: row.get::<_, i64>(0)?,
        path: row.get::<_, String>(1)?,
        line_start: row.get::<_, i64>(2)? as usize,
        line_end: row.get::<_, i64>(3)? as usize,
        text: row.get::<_, String>(4)?,
        score: row.get::<_, f64>(5).unwrap_or(0.0),
    })
}

/// Group chunk-level rows by `doc_id` into `FileSearchResult`s.
///
/// `is_better(a, b)` returns true when score `a` is better than `b` (used
/// both for picking the representative score and for sorting files).  The
/// chunks within each file are sorted by the same comparator.
fn group_by_file<F>(rows: Vec<ChunkRow>, limit: usize, is_better: F) -> Vec<FileSearchResult>
where
    F: Fn(f64, f64) -> bool + Copy,
{
    let mut by_doc: HashMap<i64, FileSearchResult> = HashMap::new();

    for r in rows {
        let entry = by_doc.entry(r.doc_id).or_insert_with(|| FileSearchResult {
            id: r.doc_id,
            path: r.path.clone(),
            score: r.score,
            chunks: Vec::new(),
        });
        if is_better(r.score, entry.score) {
            entry.score = r.score;
        }
        entry.chunks.push(ChunkHit {
            line_start: r.line_start,
            line_end: r.line_end,
            text: r.text,
            score: r.score,
        });
    }

    let mut files: Vec<FileSearchResult> = by_doc.into_values().collect();
    for f in &mut files {
        f.chunks.sort_by(|a, b| {
            if is_better(a.score, b.score) {
                std::cmp::Ordering::Less
            } else if is_better(b.score, a.score) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
    }
    files.sort_by(|a, b| {
        if is_better(a.score, b.score) {
            std::cmp::Ordering::Less
        } else if is_better(b.score, a.score) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    files.truncate(limit);
    files
}

// ── chunk helpers ─────────────────────────────────────────────────────────────

fn upsert_chunks(conn: &Connection, doc: &Document, has_vec: bool) -> Result<()> {
    // Build (line_start, line_end, embed_text) tuples.
    let computed: Vec<(usize, usize, String)>;
    let chunks: &[(usize, usize, String)] = if let Some(ref c) = doc.chunks {
        c.as_slice()
    } else {
        computed = chunk_document(&doc.body)
            .into_iter()
            .enumerate()
            .map(|(i, t)| (i, i, t))
            .collect();
        &computed
    };

    let live_starts: HashSet<i64> = chunks.iter().map(|(start, _, _)| *start as i64).collect();

    // Delete stale chunks (FTS5 external-content: need to insert 'delete' rows
    // before dropping the underlying row, or rebuild the index after).
    let old_rows: Vec<(i64, i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, line_start, text FROM chunks WHERE doc_id = ?1")?;
        stmt.query_map([doc.id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter(|(_, s, _)| !live_starts.contains(s))
        .collect()
    };
    for (cid, start, old_text) in old_rows {
        if has_vec {
            conn.execute("DELETE FROM chunk_vectors WHERE chunk_id = ?1", [cid])?;
        }
        let _ = conn.execute(
            "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
            params![cid, old_text],
        );
        conn.execute(
            "DELETE FROM chunks WHERE doc_id = ?1 AND line_start = ?2",
            params![doc.id, start],
        )?;
    }

    // Upsert each live chunk; if text changed, invalidate the stale embedding
    // and update the FTS index.
    for (line_start, line_end, text) in chunks {
        // Fetch previous row (if any) for FTS incremental delete.
        let prev: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, text FROM chunks WHERE doc_id = ?1 AND line_start = ?2",
                params![doc.id, *line_start as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .ok();

        conn.execute(
            "INSERT INTO chunks (doc_id, line_start, line_end, text)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(doc_id, line_start) DO UPDATE
             SET line_end = excluded.line_end,
                 text     = excluded.text
             WHERE text != excluded.text OR line_end != excluded.line_end",
            params![doc.id, *line_start as i64, *line_end as i64, text],
        )?;

        let new_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM chunks WHERE doc_id = ?1 AND line_start = ?2",
                params![doc.id, *line_start as i64],
                |row| row.get(0),
            )
            .ok();

        match (prev, new_id) {
            (Some((pid, old_text)), Some(nid)) if pid == nid && old_text != *text => {
                // Existing row, text changed: refresh FTS + drop stale vector.
                let _ = conn.execute(
                    "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                    params![pid, old_text],
                );
                let _ = conn.execute(
                    "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
                    params![nid, text],
                );
                if has_vec {
                    conn.execute("DELETE FROM chunk_vectors WHERE chunk_id = ?1", [nid])?;
                }
            }
            (None, Some(nid)) => {
                // New row: add to FTS.
                let _ = conn.execute(
                    "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
                    params![nid, text],
                );
            }
            _ => {}
        }
    }
    Ok(())
}

// ── sqlite-vec query helpers ──────────────────────────────────────────────────

fn sqlite_vec_embedded_keys(conn: &Connection) -> Result<HashSet<(i64, usize)>> {
    let mut stmt = conn.prepare(
        "SELECT c.doc_id, c.line_start
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
        "SELECT c.doc_id, c.line_start, c.line_end, c.text, d.path
         FROM chunks c
         JOIN documents d ON d.id = c.doc_id",
    )?;
    let chunks = stmt
        .query_map([], |row| {
            Ok(Chunk {
                doc_id: row.get::<_, i64>(0)?,
                line_start: row.get::<_, i64>(1)? as usize,
                line_end: row.get::<_, i64>(2)? as usize,
                text: row.get::<_, String>(3)?,
                doc_path: row.get::<_, String>(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .filter(|c| !embedded_keys.contains(&(c.doc_id, c.line_start)))
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
                "SELECT id FROM chunks WHERE doc_id = ?1 AND line_start = ?2",
                params![chunk.doc_id, chunk.line_start as i64],
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
