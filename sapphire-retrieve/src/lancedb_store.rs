#![cfg(feature = "lancedb-store")]
//! LanceDB vector store backend.
//!
//! Stores chunk embeddings in a LanceDB database at a caller-specified directory.
//!
//! # Chunk table schema
//!
//! | column        | type                        | notes                        |
//! |---------------|-----------------------------|------------------------------|
//! | `doc_id`      | `Int64`                     | stable document ID           |
//! | `chunk_index` | `Int32`                     | paragraph position (0-based) |
//! | `doc_title`   | `Utf8`                      | denormalised for display     |
//! | `doc_path`    | `Utf8`                      | absolute file path           |
//! | `text`        | `Utf8`                      | embeddable chunk text        |
//! | `embedding`   | `FixedSizeList<Float32, N>` | N = embedding_dim            |
//!
//! # Async boundary
//!
//! All LanceDB operations are inherently async.  [`LanceDbVectorStore`]
//! wraps them in an internal `tokio::runtime::Runtime` so that the
//! [`VectorStore`] trait remains synchronous.

use std::{
    collections::HashSet,
    path::Path,
    sync::Arc,
};

use arrow_array::{
    FixedSizeListArray, Float32Array, Int32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt as _;
use lancedb::query::{ExecutableQuery, QueryBase};

use crate::{
    error::{Error, Result},
    vector_store::{Chunk, ChunkSearchResult, VecInfo, VectorStore},
};

const TABLE_NAME: &str = "chunks";

/// Schema version for the LanceDB store.
///
/// Used to compute the versioned subdirectory:
/// `{root}/lancedb_v{LANCEDB_SCHEMA_VERSION}/`.  Bump whenever the Arrow
/// table schema changes so old data is preserved until explicitly removed.
pub const LANCEDB_SCHEMA_VERSION: i32 = 1;

/// Returns the active LanceDB store directory for the given root.
pub fn versioned_dir(root: &Path) -> std::path::PathBuf {
    root.join(format!("lancedb_v{LANCEDB_SCHEMA_VERSION}"))
}

// ── async inner ───────────────────────────────────────────────────────────────

struct LanceStore {
    table: lancedb::Table,
    dim: i32,
}

impl LanceStore {
    async fn open(data_dir: &Path, embedding_dim: u32) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let db = lancedb::connect(data_dir.to_str().unwrap_or_default())
            .execute()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?;

        let dim = embedding_dim as i32;
        let names = db
            .table_names()
            .execute()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?;

        let table = if names.contains(&TABLE_NAME.to_string()) {
            db.open_table(TABLE_NAME)
                .execute()
                .await
                .map_err(|e| Error::Embed(e.to_string()))?
        } else {
            let schema = make_schema(dim);
            let empty = RecordBatch::new_empty(schema.clone());
            db.create_table(TABLE_NAME, RecordBatchIterator::new(vec![Ok(empty)], schema))
                .execute()
                .await
                .map_err(|e| Error::Embed(e.to_string()))?
        };

        Ok(LanceStore { table, dim })
    }

    async fn embedded_chunk_keys(&self) -> Result<HashSet<(i64, usize)>> {
        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .select(lancedb::query::Select::Columns(vec![
                "doc_id".to_string(),
                "chunk_index".to_string(),
            ]))
            .execute()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?;

        let mut keys = HashSet::new();
        for batch in &batches {
            let doc_ids = batch
                .column_by_name("doc_id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
            let chunk_idxs = batch
                .column_by_name("chunk_index")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>());
            if let (Some(dids), Some(cidxs)) = (doc_ids, chunk_idxs) {
                for i in 0..batch.num_rows() {
                    keys.insert((dids.value(i), cidxs.value(i) as usize));
                }
            }
        }
        Ok(keys)
    }

    async fn insert_embeddings(&self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let schema = make_schema(self.dim);

        let doc_ids: Vec<i64> = chunks.iter().map(|c| c.doc_id).collect();
        let chunk_idxs: Vec<i32> = chunks.iter().map(|c| c.chunk_index as i32).collect();
        let titles: Vec<&str> = chunks.iter().map(|c| c.doc_title.as_str()).collect();
        let paths: Vec<&str> = chunks.iter().map(|c| c.doc_path.as_str()).collect();
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(doc_ids)),
                Arc::new(Int32Array::from(chunk_idxs)),
                Arc::new(StringArray::from(titles)),
                Arc::new(StringArray::from(paths)),
                Arc::new(StringArray::from(texts)),
                Arc::new(make_embedding_array(embeddings, self.dim)?),
            ],
        )
        .map_err(|e| Error::Embed(e.to_string()))?;

        self.table
            .add(RecordBatchIterator::new(vec![Ok(batch)], schema))
            .execute()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?;
        Ok(())
    }

    async fn search_similar(
        &self,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<ChunkSearchResult>> {
        let batches: Vec<RecordBatch> = self
            .table
            .vector_search(query_vec)
            .map_err(|e| Error::Embed(e.to_string()))?
            .column("embedding")
            .limit(limit)
            .execute()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?
            .try_collect()
            .await
            .map_err(|e| Error::Embed(e.to_string()))?;

        let mut results = Vec::new();
        for batch in &batches {
            let doc_ids = batch
                .column_by_name("doc_id")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                .ok_or_else(|| Error::Embed("missing `doc_id` in search result".into()))?;
            let chunk_idxs = batch
                .column_by_name("chunk_index")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
                .ok_or_else(|| Error::Embed("missing `chunk_index` in search result".into()))?;
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
                results.push(ChunkSearchResult {
                    doc_id: doc_ids.value(i),
                    chunk_index: chunk_idxs.value(i) as usize,
                    doc_title: titles.value(i).to_owned(),
                    doc_path: paths.value(i).to_owned(),
                    chunk_text: texts.value(i).to_owned(),
                    score: dists.value(i) as f64,
                });
            }
        }
        Ok(results)
    }

    async fn embedded_count(&self) -> u64 {
        self.table.count_rows(None).await.unwrap_or(0) as u64
    }
}

// ── public sync wrapper ───────────────────────────────────────────────────────

/// Vector store backed by LanceDB.
///
/// Wraps the async [`LanceStore`] in an internal Tokio runtime so that it
/// implements the synchronous [`VectorStore`] trait.
pub struct LanceDbVectorStore {
    inner: LanceStore,
    rt: tokio::runtime::Runtime,
}

impl LanceDbVectorStore {
    /// Open (or create) the LanceDB vector store at `data_dir` with
    /// `embedding_dim` dimensions.
    pub fn new(data_dir: &Path, embedding_dim: u32) -> Result<Self> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Embed(format!("failed to create Tokio runtime: {e}")))?;
        let inner = rt.block_on(LanceStore::open(data_dir, embedding_dim))?;
        Ok(Self { inner, rt })
    }

    /// Read vector index statistics.
    ///
    /// `sqlite_conn` must be a connection to the retrieve SQLite database
    /// (which holds the `chunks` table used to compute the pending count).
    pub fn vec_info(&self, sqlite_conn: &rusqlite::Connection) -> Result<VecInfo> {
        let vector_count = self.rt.block_on(self.inner.embedded_count());
        let chunk_count: u64 = sqlite_conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as u64;
        Ok(VecInfo {
            embedding_dim: self.inner.dim as u32,
            vector_count,
            pending_count: chunk_count.saturating_sub(vector_count),
        })
    }
}

impl VectorStore for LanceDbVectorStore {
    fn embedded_chunk_keys(&self) -> Result<HashSet<(i64, usize)>> {
        self.rt.block_on(self.inner.embedded_chunk_keys())
    }

    fn insert_embeddings(&self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()> {
        self.rt.block_on(self.inner.insert_embeddings(chunks, embeddings))
    }

    fn search_similar(&self, query_vec: &[f32], limit: usize) -> Result<Vec<ChunkSearchResult>> {
        self.rt.block_on(self.inner.search_similar(query_vec, limit))
    }
}

// ── Arrow helpers ─────────────────────────────────────────────────────────────

fn make_schema(dim: i32) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("doc_id", DataType::Int64, false),
        Field::new("chunk_index", DataType::Int32, false),
        Field::new("doc_title", DataType::Utf8, false),
        Field::new("doc_path", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim,
            ),
            false,
        ),
    ]))
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
