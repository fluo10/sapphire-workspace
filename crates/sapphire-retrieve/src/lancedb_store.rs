#![cfg(feature = "lancedb-store")]
//! Full LanceDB backend for [`RetrieveDb`].
//!
//! When LanceDB is selected as the vector backend, this module stores *all*
//! data — files, documents, chunks, and embeddings — in LanceDB tables.
//!
//! # Tables
//!
//! | table           | columns                                                                                          |
//! |-----------------|--------------------------------------------------------------------------------------------------|
//! | `files`         | `path Utf8, mtime Int64`                                                                         |
//! | `documents`     | `id Int64, title Utf8, path Utf8`                                                                |
//! | `chunks_meta`   | `doc_id Int64, line_start Int32, line_end Int32, text Utf8, doc_title Utf8, doc_path Utf8`       |
//! | `chunk_vectors` | `doc_id Int64, line_start Int32, line_end Int32, doc_title Utf8, doc_path Utf8, text Utf8, embedding FixedSizeList<Float32>` |
//!
//! FTS is built on `chunks_meta.text` (chunk-level).

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use arrow_array::{
    FixedSizeListArray, Float32Array, Int32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt as _;
use lancedb::{
    index::{
        Index,
        scalar::{FtsIndexBuilder, FullTextSearchQuery},
    },
    query::{ExecutableQuery, QueryBase, Select},
};

use crate::{
    chunker::chunk_document,
    embed::Embedder,
    error::{Error, Result},
    retrieve_store::{
        ChunkHit, Document, FileSearchResult, FtsQuery, RetrieveStore, VectorQuery,
    },
    vector_store::{Chunk, VecInfo},
};

// ── versioning ────────────────────────────────────────────────────────────────

/// Schema version encoded in the directory name.
///
/// Version history:
/// - 1: initial schema
/// - 2: LanceDB full-backend
/// - 3: replace `chunk_index` with `line` + `col` (source positions)
/// - 4: chunk-level FTS, `line_start`/`line_end`, drop `documents.body`
pub const SCHEMA_VERSION: i32 = 4;

/// Returns the full-backend LanceDB directory for the given cache root.
pub fn data_dir(root: &Path) -> PathBuf {
    root.join(format!("lancedb_v{SCHEMA_VERSION}"))
}

// ── table names ───────────────────────────────────────────────────────────────

const TBL_FILES: &str = "files";
const TBL_DOCUMENTS: &str = "documents";
const TBL_CHUNKS_META: &str = "chunks_meta";
const TBL_CHUNK_VECTORS: &str = "chunk_vectors";

// ── Arrow schemas ─────────────────────────────────────────────────────────────

fn files_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("mtime", DataType::Int64, false),
    ]))
}

fn documents_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
    ]))
}

fn chunks_meta_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("doc_id", DataType::Int64, false),
        Field::new("line_start", DataType::Int32, false),
        Field::new("line_end", DataType::Int32, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("doc_title", DataType::Utf8, false),
        Field::new("doc_path", DataType::Utf8, false),
    ]))
}

fn chunk_vectors_schema(dim: i32) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("doc_id", DataType::Int64, false),
        Field::new("line_start", DataType::Int32, false),
        Field::new("line_end", DataType::Int32, false),
        Field::new("doc_title", DataType::Utf8, false),
        Field::new("doc_path", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            false,
        ),
    ]))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

async fn open_or_create(
    db: &lancedb::Connection,
    name: &str,
    schema: Arc<Schema>,
) -> Result<lancedb::Table> {
    let names = db.table_names().execute().await?;
    if names.contains(&name.to_string()) {
        Ok(db.open_table(name).execute().await?)
    } else {
        let empty = RecordBatch::new_empty(schema);
        Ok(db.create_table(name, empty).execute().await?)
    }
}

fn make_embedding_array(embeddings: &[Vec<f32>], dim: i32) -> Result<FixedSizeListArray> {
    let flat: Vec<f32> = embeddings.iter().flat_map(|v| v.iter().copied()).collect();
    let values = Arc::new(Float32Array::from(flat));
    FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dim,
        values,
        None,
    )
    .map_err(|e| Error::Embed(e.to_string()))
}

// ── async inner ───────────────────────────────────────────────────────────────

struct LanceFullStore {
    files: lancedb::Table,
    documents: lancedb::Table,
    chunks_meta: lancedb::Table,
    chunk_vectors: lancedb::Table,
    dim: i32,
}

impl LanceFullStore {
    async fn open(data_dir: &Path, embedding_dim: u32) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let db = lancedb::connect(data_dir.to_str().unwrap_or_default())
            .execute()
            .await?;
        let dim = embedding_dim as i32;

        let files = open_or_create(&db, TBL_FILES, files_schema()).await?;
        let documents = open_or_create(&db, TBL_DOCUMENTS, documents_schema()).await?;
        let chunks_meta = open_or_create(&db, TBL_CHUNKS_META, chunks_meta_schema()).await?;
        let chunk_vectors =
            open_or_create(&db, TBL_CHUNK_VECTORS, chunk_vectors_schema(dim)).await?;

        Ok(Self {
            files,
            documents,
            chunks_meta,
            chunk_vectors,
            dim,
        })
    }

    // ── file tracking ─────────────────────────────────────────────────────────

    async fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        let batches: Vec<RecordBatch> = self.files.query().execute().await?.try_collect().await?;

        let mut map = HashMap::new();
        for batch in &batches {
            let paths = batch
                .column_by_name("path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let mtimes = batch
                .column_by_name("mtime")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
            if let (Some(ps), Some(ms)) = (paths, mtimes) {
                for i in 0..batch.num_rows() {
                    map.insert(ps.value(i).to_owned(), ms.value(i));
                }
            }
        }
        Ok(map)
    }

    async fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        let schema = files_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec![path])),
                Arc::new(Int64Array::from(vec![mtime])),
            ],
        )
        .map_err(|e| Error::Embed(e.to_string()))?;

        let mut merge = self.files.merge_insert(&["path"]);
        merge
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge
            .execute(Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema)))
            .await?;
        Ok(())
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        let safe = escape_sql_string(path);
        self.files.delete(&format!("path = '{safe}'")).await?;
        Ok(())
    }

    async fn file_count(&self) -> Result<u64> {
        Ok(self.files.count_rows(None).await? as u64)
    }

    // ── document management ───────────────────────────────────────────────────

    async fn upsert_document(&self, doc: &Document) -> Result<()> {
        let schema = documents_schema();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![doc.id])),
                Arc::new(StringArray::from(vec![doc.title.as_str()])),
                Arc::new(StringArray::from(vec![doc.path.as_str()])),
            ],
        )
        .map_err(|e| Error::Embed(e.to_string()))?;

        let mut merge = self.documents.merge_insert(&["id"]);
        merge
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge
            .execute(Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema)))
            .await?;

        // Remove stale chunks.
        self.chunks_meta
            .delete(&format!("doc_id = {}", doc.id))
            .await?;
        self.chunk_vectors
            .delete(&format!("doc_id = {}", doc.id))
            .await?;

        // Build (line_start, line_end, embed_text) tuples.
        let computed: Vec<(usize, usize, String)>;
        let chunks: &[(usize, usize, String)] = if let Some(ref c) = doc.chunks {
            c.as_slice()
        } else {
            computed = chunk_document(&doc.title, &doc.body)
                .into_iter()
                .enumerate()
                .map(|(i, t)| (i, i, t))
                .collect();
            &computed
        };

        if chunks.is_empty() {
            return Ok(());
        }

        let schema = chunks_meta_schema();
        let n = chunks.len();
        let doc_ids = vec![doc.id; n];
        let line_starts: Vec<i32> = chunks.iter().map(|(s, _, _)| *s as i32).collect();
        let line_ends: Vec<i32> = chunks.iter().map(|(_, e, _)| *e as i32).collect();
        let titles = vec![doc.title.as_str(); n];
        let paths = vec![doc.path.as_str(); n];
        let texts: Vec<&str> = chunks.iter().map(|(_, _, t)| t.as_str()).collect();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(doc_ids)),
                Arc::new(Int32Array::from(line_starts)),
                Arc::new(Int32Array::from(line_ends)),
                Arc::new(StringArray::from(texts)),
                Arc::new(StringArray::from(titles)),
                Arc::new(StringArray::from(paths)),
            ],
        )
        .map_err(|e| Error::Embed(e.to_string()))?;

        self.chunks_meta.add(vec![batch]).execute().await?;

        Ok(())
    }

    async fn remove_document(&self, id: i64) -> Result<()> {
        self.documents.delete(&format!("id = {id}")).await?;
        self.chunks_meta.delete(&format!("doc_id = {id}")).await?;
        self.chunk_vectors.delete(&format!("doc_id = {id}")).await?;
        Ok(())
    }

    async fn rebuild_fts(&self) -> Result<()> {
        self.chunks_meta
            .create_index(
                &["text"],
                Index::FTS(FtsIndexBuilder::default().base_tokenizer("ngram".to_owned())),
            )
            .replace(true)
            .execute()
            .await?;
        Ok(())
    }

    // ── search ────────────────────────────────────────────────────────────────

    async fn search_fts(
        &self,
        query: &str,
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<FileSearchResult>> {
        let over_fetch = limit.saturating_mul(5);
        let mut qb = self
            .chunks_meta
            .query()
            .full_text_search(FullTextSearchQuery::new(query.to_owned()))
            .select(Select::Columns(vec![
                "doc_id".to_string(),
                "doc_title".to_string(),
                "doc_path".to_string(),
                "line_start".to_string(),
                "line_end".to_string(),
                "text".to_string(),
            ]))
            .limit(over_fetch);
        if let Some(pfx) = path_prefix {
            qb = qb.only_if(format!("doc_path LIKE '{}%'", escape_sql_string(pfx)));
        }
        let batches: Vec<RecordBatch> = qb.execute().await?.try_collect().await?;

        let mut rows = Vec::new();
        for batch in &batches {
            let doc_ids = batch
                .column_by_name("doc_id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
            let titles = batch
                .column_by_name("doc_title")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let paths = batch
                .column_by_name("doc_path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let line_starts = batch
                .column_by_name("line_start")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>());
            let line_ends = batch
                .column_by_name("line_end")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>());
            let texts = batch
                .column_by_name("text")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let scores = batch
                .column_by_name("_score")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

            if let (Some(dids), Some(ttls), Some(pths), Some(ls), Some(le), Some(txts)) =
                (doc_ids, titles, paths, line_starts, line_ends, texts)
            {
                for i in 0..batch.num_rows() {
                    rows.push(RawHit {
                        doc_id: dids.value(i),
                        title: ttls.value(i).to_owned(),
                        path: pths.value(i).to_owned(),
                        line_start: ls.value(i) as usize,
                        line_end: le.value(i) as usize,
                        text: txts.value(i).to_owned(),
                        score: scores.map_or(0.0, |s| s.value(i) as f64),
                    });
                }
            }
        }
        // BM25 _score: higher = more relevant.
        Ok(group_by_file(rows, limit, |a, b| a > b))
    }

    async fn search_similar(
        &self,
        query_vec: &[f32],
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<FileSearchResult>> {
        let over_fetch = limit.saturating_mul(5);
        let mut qb = self
            .chunk_vectors
            .vector_search(query_vec)
            .map_err(|e| Error::Embed(e.to_string()))?
            .column("embedding")
            .limit(over_fetch);
        if let Some(pfx) = path_prefix {
            qb = qb.only_if(format!("doc_path LIKE '{}%'", escape_sql_string(pfx)));
        }
        let batches: Vec<RecordBatch> = qb.execute().await?.try_collect().await?;

        let mut rows = Vec::new();
        for batch in &batches {
            let doc_ids = batch
                .column_by_name("doc_id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                .ok_or_else(|| Error::Embed("missing `doc_id` in search result".into()))?;
            let line_starts = batch
                .column_by_name("line_start")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
                .ok_or_else(|| Error::Embed("missing `line_start` in search result".into()))?;
            let line_ends = batch
                .column_by_name("line_end")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
                .ok_or_else(|| Error::Embed("missing `line_end` in search result".into()))?;
            let titles = batch
                .column_by_name("doc_title")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or_else(|| Error::Embed("missing `doc_title` in search result".into()))?;
            let paths = batch
                .column_by_name("doc_path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or_else(|| Error::Embed("missing `doc_path` in search result".into()))?;
            let texts = batch
                .column_by_name("text")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or_else(|| Error::Embed("missing `text` in search result".into()))?;
            let dists = batch
                .column_by_name("_distance")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
                .ok_or_else(|| Error::Embed("missing `_distance` in search result".into()))?;

            for i in 0..batch.num_rows() {
                rows.push(RawHit {
                    doc_id: doc_ids.value(i),
                    title: titles.value(i).to_owned(),
                    path: paths.value(i).to_owned(),
                    line_start: line_starts.value(i) as usize,
                    line_end: line_ends.value(i) as usize,
                    text: texts.value(i).to_owned(),
                    score: dists.value(i) as f64,
                });
            }
        }
        // Vector distance: lower = better.
        Ok(group_by_file(rows, limit, |a, b| a < b))
    }

    // ── embedding ─────────────────────────────────────────────────────────────

    async fn embedded_chunk_keys(&self) -> Result<HashSet<(i64, usize)>> {
        let batches: Vec<RecordBatch> = self
            .chunk_vectors
            .query()
            .select(Select::Columns(vec![
                "doc_id".to_string(),
                "line_start".to_string(),
            ]))
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut keys = HashSet::new();
        for batch in &batches {
            let doc_ids = batch
                .column_by_name("doc_id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
            let line_starts = batch
                .column_by_name("line_start")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>());
            if let (Some(dids), Some(ls)) = (doc_ids, line_starts) {
                for i in 0..batch.num_rows() {
                    keys.insert((dids.value(i), ls.value(i) as usize));
                }
            }
        }
        Ok(keys)
    }

    async fn pending_chunks(&self, embedded: &HashSet<(i64, usize)>) -> Result<Vec<Chunk>> {
        let batches: Vec<RecordBatch> = self
            .chunks_meta
            .query()
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut chunks = Vec::new();
        for batch in &batches {
            let doc_ids = batch
                .column_by_name("doc_id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
            let line_starts = batch
                .column_by_name("line_start")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>());
            let line_ends = batch
                .column_by_name("line_end")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>());
            let texts = batch
                .column_by_name("text")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let titles = batch
                .column_by_name("doc_title")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let paths = batch
                .column_by_name("doc_path")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            if let (Some(dids), Some(ls), Some(le), Some(txts), Some(ttls), Some(pths)) =
                (doc_ids, line_starts, line_ends, texts, titles, paths)
            {
                for i in 0..batch.num_rows() {
                    let key = (dids.value(i), ls.value(i) as usize);
                    if !embedded.contains(&key) {
                        chunks.push(Chunk {
                            doc_id: key.0,
                            line_start: key.1,
                            line_end: le.value(i) as usize,
                            text: txts.value(i).to_owned(),
                            doc_title: ttls.value(i).to_owned(),
                            doc_path: pths.value(i).to_owned(),
                        });
                    }
                }
            }
        }
        Ok(chunks)
    }

    async fn insert_embeddings(&self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let schema = chunk_vectors_schema(self.dim);
        let doc_ids: Vec<i64> = chunks.iter().map(|c| c.doc_id).collect();
        let line_starts: Vec<i32> = chunks.iter().map(|c| c.line_start as i32).collect();
        let line_ends: Vec<i32> = chunks.iter().map(|c| c.line_end as i32).collect();
        let titles: Vec<&str> = chunks.iter().map(|c| c.doc_title.as_str()).collect();
        let paths: Vec<&str> = chunks.iter().map(|c| c.doc_path.as_str()).collect();
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(doc_ids)),
                Arc::new(Int32Array::from(line_starts)),
                Arc::new(Int32Array::from(line_ends)),
                Arc::new(StringArray::from(titles)),
                Arc::new(StringArray::from(paths)),
                Arc::new(StringArray::from(texts)),
                Arc::new(make_embedding_array(embeddings, self.dim)?),
            ],
        )
        .map_err(|e| Error::Embed(e.to_string()))?;

        self.chunk_vectors.add(vec![batch]).execute().await?;
        Ok(())
    }

    // ── info ──────────────────────────────────────────────────────────────────

    async fn vec_info(&self, dim: u32) -> Result<VecInfo> {
        let chunk_count = self.chunks_meta.count_rows(None).await? as u64;
        let vector_count = self.chunk_vectors.count_rows(None).await? as u64;
        Ok(VecInfo {
            embedding_dim: dim,
            vector_count,
            pending_count: chunk_count.saturating_sub(vector_count),
        })
    }

    async fn document_ids(&self) -> Result<Vec<i64>> {
        let batches: Vec<RecordBatch> = self
            .documents
            .query()
            .select(Select::Columns(vec!["id".to_string()]))
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut ids = Vec::new();
        for batch in &batches {
            if let Some(col) = batch
                .column_by_name("id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            {
                for i in 0..batch.num_rows() {
                    ids.push(col.value(i));
                }
            }
        }
        Ok(ids)
    }

    async fn document_count(&self) -> Result<u64> {
        Ok(self.documents.count_rows(None).await? as u64)
    }
}

// ── raw hit + grouping ────────────────────────────────────────────────────────

struct RawHit {
    doc_id: i64,
    title: String,
    path: String,
    line_start: usize,
    line_end: usize,
    text: String,
    score: f64,
}

fn group_by_file<F>(rows: Vec<RawHit>, limit: usize, is_better: F) -> Vec<FileSearchResult>
where
    F: Fn(f64, f64) -> bool + Copy,
{
    let mut by_doc: HashMap<i64, FileSearchResult> = HashMap::new();

    for r in rows {
        let entry = by_doc.entry(r.doc_id).or_insert_with(|| FileSearchResult {
            id: r.doc_id,
            title: r.title.clone(),
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

// ── public sync wrapper ───────────────────────────────────────────────────────

pub(crate) struct LanceDbBackend {
    inner: LanceFullStore,
    rt: tokio::runtime::Runtime,
    dim: u32,
}

impl LanceDbBackend {
    pub fn new(data_dir: &Path, embedding_dim: u32) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Embed(format!("failed to create Tokio runtime: {e}")))?;
        let inner = Self::block_on_with(&rt, LanceFullStore::open(data_dir, embedding_dim))?;
        Ok(Self {
            inner,
            rt,
            dim: embedding_dim,
        })
    }

    fn block_on_with<F: std::future::Future>(rt: &tokio::runtime::Runtime, f: F) -> F::Output {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(f)),
            Err(_) => rt.block_on(f),
        }
    }

    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        Self::block_on_with(&self.rt, f)
    }

    // ── file tracking ─────────────────────────────────────────────────────────

    pub fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        self.block_on(self.inner.file_mtimes())
    }

    pub fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        self.block_on(self.inner.upsert_file(path, mtime))
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.block_on(self.inner.remove_file(path))
    }

    pub fn file_count(&self) -> Result<u64> {
        self.block_on(self.inner.file_count())
    }

    // ── document management ───────────────────────────────────────────────────

    pub fn upsert_document(&self, doc: &Document) -> Result<()> {
        self.block_on(self.inner.upsert_document(doc))
    }

    pub fn remove_document(&self, id: i64) -> Result<()> {
        self.block_on(self.inner.remove_document(id))
    }

    pub fn rebuild_fts(&self) -> Result<()> {
        self.block_on(self.inner.rebuild_fts())
    }

    pub fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<FileSearchResult>> {
        let pfx = q.path_prefix.map(|p| p.to_string_lossy().to_string());
        self.block_on(self.inner.search_fts(q.query, q.limit, pfx.as_deref()))
    }

    // ── vector search ─────────────────────────────────────────────────────────

    pub fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<FileSearchResult>> {
        let query_vecs = q.embedder.embed_texts(&[q.query])?;
        let query_vec = query_vecs
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embed("embedder returned empty result".into()))?;
        let pfx = q.path_prefix.map(|p| p.to_string_lossy().to_string());
        self.block_on(
            self.inner
                .search_similar(&query_vec, q.limit, pfx.as_deref()),
        )
    }

    // ── embedding ─────────────────────────────────────────────────────────────

    pub fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: impl Fn(usize, usize),
    ) -> Result<usize> {
        let embedded = self.block_on(self.inner.embedded_chunk_keys())?;
        let pending = self.block_on(self.inner.pending_chunks(&embedded))?;
        let total = pending.len();
        let mut done = 0;

        for batch in pending.chunks(100) {
            let texts: Vec<&str> = batch.iter().map(|c| c.text.as_str()).collect();
            let embeddings = embedder.embed_texts(&texts)?;
            self.block_on(self.inner.insert_embeddings(batch, &embeddings))?;
            done += batch.len();
            on_progress(done, total);
        }
        Ok(total)
    }

    // ── info ──────────────────────────────────────────────────────────────────

    pub fn vec_info(&self) -> Result<VecInfo> {
        self.block_on(self.inner.vec_info(self.dim))
    }

    pub fn document_ids(&self) -> Result<Vec<i64>> {
        self.block_on(self.inner.document_ids())
    }

    pub fn document_count(&self) -> Result<u64> {
        self.block_on(self.inner.document_count())
    }
}

// ── RetrieveStore impl ────────────────────────────────────────────────────────

impl RetrieveStore for LanceDbBackend {
    fn file_mtimes(&self) -> Result<HashMap<String, i64>> {
        self.file_mtimes()
    }

    fn upsert_file(&self, path: &str, mtime: i64) -> Result<()> {
        self.upsert_file(path, mtime)
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.remove_file(path)
    }

    fn file_count(&self) -> Result<u64> {
        self.file_count()
    }

    fn upsert_document(&self, doc: &Document) -> Result<()> {
        self.upsert_document(doc)
    }

    fn remove_document(&self, id: i64) -> Result<()> {
        self.remove_document(id)
    }

    fn rebuild_fts(&self) -> Result<()> {
        self.rebuild_fts()
    }

    fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<FileSearchResult>> {
        self.search_fts(q)
    }

    fn document_ids(&self) -> Result<Vec<i64>> {
        self.document_ids()
    }

    fn document_count(&self) -> Result<u64> {
        self.document_count()
    }

    fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<usize> {
        self.embed_pending(embedder, on_progress)
    }

    fn vec_info(&self) -> Result<VecInfo> {
        self.vec_info()
    }

    fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<FileSearchResult>> {
        self.search_similar(q)
    }
}
