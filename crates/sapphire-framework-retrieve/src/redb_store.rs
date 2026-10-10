//! Pure-Rust backend for [`RetrieveStore`]: **redb + tantivy**.
//!
//! [`RedbStore`] keeps all state in a directory, with **no C dependency** (no
//! SQLite / libsqlite3-sys), so downstream binaries are never tied to another
//! crate's rusqlite version.
//!
//! Layout (`<dir>/`):
//!
//! | path | role |
//! |------|------|
//! | `docs.redb` | canonical record store (one record + one vector per file, meta) |
//! | `tantivy/`  | full-text inverted index (derived, rebuildable from redb) |
//!
//! - **redb** holds the source-of-truth cache records. `documents` maps
//!   `doc_id -> {path, text}`; `vectors` maps `doc_id -> f32[]`; `meta` holds
//!   `embedding_model`, `embedding_dim` and `schema_version`.
//! - **tantivy** holds a trigram full-text index (BM25) with one document per
//!   file. It is derived from redb and can be rebuilt at any time.
//! - **Vector search is brute-force** over the `vectors` table (exact, no ANN).
//!   Fine up to tens of thousands of files; swap in an HNSW index later if the
//!   collection grows.
//! - A store whose `schema_version` is not `SCHEMA_VERSION` (an older shape)
//!   is wiped on open and rebuilt by the caller, as for a fresh cache.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term,
    collector::TopDocs,
    doc,
    query::QueryParser,
    schema::{
        Field, INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value,
    },
    snippet::SnippetGenerator,
    tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer},
};

use crate::{
    embed::Embedder,
    error::{Error, Result},
    retrieve_store::{
        Document, FileSearchResult, FtsQuery, RetrieveStore, VectorQuery, VectorSource,
    },
    snippet::{collapse_and_cut, leading},
    vector_store::{VecInfo, l2_distance, vec_deserialize, vec_serialize},
};

// ── redb tables ────────────────────────────────────────────────────────────────

/// `doc_id -> serde_json(DocRecord)`.
const DOCUMENTS: TableDefinition<i64, &[u8]> = TableDefinition::new("documents");
/// `doc_id (i64 LE) -> little-endian f32 blob`.
const VECTORS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("vectors");
/// misc key/value metadata (`embedding_model`, `embedding_dim`, `schema_version`).
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// The store's on-disk shape. 1: chunked records (no key). 2: one record per file.
const SCHEMA_VERSION: u32 = 2;

#[derive(serde::Serialize, serde::Deserialize)]
struct DocRecord {
    path: String,
    text: String,
}

fn vkey(doc_id: i64) -> [u8; 8] {
    doc_id.to_le_bytes()
}

fn vkey_parse(b: &[u8]) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(..8)?.try_into().ok()?))
}

// ── error mapping ────────────────────────────────────────────────────────────

fn redb_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Redb(e.to_string())
}

/// Open `dir/docs.redb`, clearing the whole store directory first if the
/// database was written in a redb file format this build can no longer read.
///
/// redb 3 dropped support for the v2 on-disk format, so a file written by
/// redb 2 answers [`redb::DatabaseError::UpgradeRequired`] instead of opening.
/// The redb records and the tantivy index beside them have to describe the
/// same documents, so the recovery clears the directory rather than just the
/// unreadable file — leaving a populated tantivy index next to empty redb
/// tables would surface hits for documents the store can no longer produce.
/// The caller then re-indexes exactly as it would for a cache that had never
/// existed.
fn create_or_reset(dir: &Path) -> Result<Database> {
    let path = dir.join("docs.redb");
    match Database::create(&path) {
        Err(redb::DatabaseError::UpgradeRequired(_)) => {
            std::fs::remove_dir_all(dir)?;
            std::fs::create_dir_all(dir)?;
            Database::create(&path).map_err(redb_err)
        }
        other => other.map_err(redb_err),
    }
}
fn tantivy_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Tantivy(e.to_string())
}

// ── redb helpers (usable before a `RedbStore` exists) ───────────────────────────

/// Create every table, so later read transactions can open them.
fn create_tables(db: &Database) -> Result<()> {
    let wtx = db.begin_write().map_err(redb_err)?;
    wtx.open_table(DOCUMENTS).map_err(redb_err)?;
    wtx.open_table(VECTORS).map_err(redb_err)?;
    wtx.open_table(META).map_err(redb_err)?;
    wtx.commit().map_err(redb_err)?;
    Ok(())
}

fn read_meta_u32(db: &Database, key: &str) -> Result<Option<u32>> {
    let rtx = db.begin_read().map_err(redb_err)?;
    let t = rtx.open_table(META).map_err(redb_err)?;
    let v = t.get(key).map_err(redb_err)?;
    Ok(v.and_then(|g| {
        let b = g.value();
        (b.len() == 4).then(|| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }))
}

fn write_meta_u32(db: &Database, key: &str, value: u32) -> Result<()> {
    let wtx = db.begin_write().map_err(redb_err)?;
    {
        let mut t = wtx.open_table(META).map_err(redb_err)?;
        t.insert(key, value.to_le_bytes().as_slice())
            .map_err(redb_err)?;
    }
    wtx.commit().map_err(redb_err)?;
    Ok(())
}

/// Whether the store holds no documents (a fresh cache).
fn is_empty(db: &Database) -> Result<bool> {
    let rtx = db.begin_read().map_err(redb_err)?;
    let t = rtx.open_table(DOCUMENTS).map_err(redb_err)?;
    Ok(t.len().map_err(redb_err)? == 0)
}

/// How much of a file's text, in bytes, the FTS snippet generator looks at.
///
/// `SnippetGenerator::snippet` tokenizes the whole text it is given, which is
/// costly for a large file on every hit. Only the first 256 KiB are passed (cut
/// on a `char` boundary); a match past that point gets the file's leading text
/// as its snippet instead.
const SNIPPET_SOURCE_BYTES: usize = 256 * 1024;

/// `text` cut to at most `max` bytes, on a `char` boundary.
fn snippet_source(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The snippet for an FTS hit: the generator's fragment when it has one, else
/// the file's leading text.
fn pick_snippet(fragment: &str, text: &str) -> String {
    if fragment.trim().is_empty() {
        leading(text)
    } else {
        collapse_and_cut(fragment)
    }
}

// ── tantivy schema ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Fields {
    doc_id: Field,
    text: Field,
}

const TRIGRAM_TOKENIZER: &str = "trigram";

/// Build the tantivy schema. `text` is indexed with a character-trigram
/// tokenizer (mirrors the previous SQLite FTS5 `trigram` design, so substring
/// and CJK matching keep working); `doc_id` is stored so hits can be resolved
/// back to redb records, and indexed for `delete_term`.
fn build_schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();
    let doc_id = sb.add_i64_field("doc_id", INDEXED | STORED);
    let text_indexing = TextFieldIndexing::default()
        .set_tokenizer(TRIGRAM_TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text_opts = TextOptions::default().set_indexing_options(text_indexing);
    let text = sb.add_text_field("text", text_opts);
    let schema = sb.build();
    (schema, Fields { doc_id, text })
}

fn register_trigram(index: &Index) -> Result<()> {
    let ngram = NgramTokenizer::new(3, 3, false).map_err(tantivy_err)?;
    let analyzer = TextAnalyzer::builder(ngram).filter(LowerCaser).build();
    index.tokenizers().register(TRIGRAM_TOKENIZER, analyzer);
    Ok(())
}

// ── RedbStore ────────────────────────────────────────────────────────────────

pub struct RedbStore {
    db: Arc<Database>,
    index: Index,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    fields: Fields,
    /// The vector dimension; `None` while vector search is off. Interior-mutable
    /// so [`RetrieveStore::configure_vectors`] can turn vectors on (or switch
    /// model) on a store that is already open and shared.
    dim: Mutex<Option<u32>>,
}

impl RedbStore {
    /// Open (or create) a store at `dir`. `dim` enables vector search when set
    /// (it writes `embedding_dim` only; see [`RetrieveStore::configure_vectors`]
    /// to also record the model).
    pub fn open(dir: &Path, dim: Option<u32>) -> Result<Self> {
        std::fs::create_dir_all(dir)?;

        // redb
        let db = create_or_reset(dir)?;
        create_tables(&db)?;

        // Schema version. This runs before the tantivy directory is opened, so a
        // wipe clears the index along with the records.
        let db = match read_meta_u32(&db, "schema_version")? {
            Some(SCHEMA_VERSION) => db,
            _ if is_empty(&db)? => {
                // Fresh, or an older store with no documents: keep the records
                // (and `embedding_dim`), but drop any index an older build created,
                // whose tantivy schema would no longer match.
                match std::fs::remove_dir_all(dir.join("tantivy")) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
                db
            }
            // A newer schema version is wiped too, on purpose: the store is only a cache.
            _ => {
                // A cache written in an older shape: start over, as for UpgradeRequired.
                drop(db);
                std::fs::remove_dir_all(dir)?;
                std::fs::create_dir_all(dir)?;
                let db = create_or_reset(dir)?;
                create_tables(&db)?;
                db
            }
        };
        write_meta_u32(&db, "schema_version", SCHEMA_VERSION)?;

        // tantivy
        let tantivy_dir = dir.join("tantivy");
        std::fs::create_dir_all(&tantivy_dir)?;
        let (schema, fields) = build_schema();
        let mmap = tantivy::directory::MmapDirectory::open(&tantivy_dir).map_err(tantivy_err)?;
        let index = Index::open_or_create(mmap, schema).map_err(tantivy_err)?;
        register_trigram(&index)?;
        let writer: IndexWriter = index.writer(50_000_000).map_err(tantivy_err)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(tantivy_err)?;

        let store = Self {
            db: Arc::new(db),
            index,
            writer: Mutex::new(writer),
            reader,
            fields,
            dim: Mutex::new(dim),
        };

        if let Some(d) = dim {
            store.set_meta_u32("embedding_dim", d)?;
        } else if let Some(d) = store.get_meta_u32("embedding_dim")? {
            // Re-open with a previously configured dim.
            *store.dim.lock().unwrap() = Some(d);
        }
        Ok(store)
    }

    fn set_meta_u32(&self, key: &str, value: u32) -> Result<()> {
        write_meta_u32(&self.db, key, value)
    }

    fn get_meta_u32(&self, key: &str) -> Result<Option<u32>> {
        read_meta_u32(&self.db, key)
    }

    #[cfg(test)]
    fn clear_meta_for_test(&self, key: &str) {
        let wtx = self.db.begin_write().unwrap();
        {
            wtx.open_table(META).unwrap().remove(key).unwrap();
        }
        wtx.commit().unwrap();
    }

    pub fn dim(&self) -> Option<u32> {
        *self.dim.lock().unwrap()
    }

    /// Turn vector search on with `dim`, in place, writing `embedding_dim` only
    /// (the same as `open(dir, Some(dim))`, without opening the store again).
    /// Legacy: it does not clear vectors of another model or size; callers should
    /// use [`RetrieveStore::configure_vectors`], which also records the model.
    pub fn set_dim(&self, dim: u32) -> Result<()> {
        let mut cur = self.dim.lock().unwrap();
        self.set_meta_u32("embedding_dim", dim)?;
        *cur = Some(dim);
        Ok(())
    }

    fn get_doc(&self, doc_id: i64) -> Result<Option<DocRecord>> {
        let rtx = self.db.begin_read().map_err(redb_err)?;
        let t = rtx.open_table(DOCUMENTS).map_err(redb_err)?;
        let v = t.get(doc_id).map_err(redb_err)?;
        Ok(v.and_then(|g| serde_json::from_slice(g.value()).ok()))
    }

    /// Store one vector per document in a single write transaction.
    ///
    /// A vector whose length is not the store's current dimension is dropped
    /// with a warning (the document stays pending): it comes from a run that
    /// overlapped a [`RetrieveStore::configure_vectors`] switch to another model.
    /// Returns how many vectors were stored.
    fn put_vectors(&self, vectors: &[(i64, Vec<f32>)]) -> Result<usize> {
        if vectors.is_empty() {
            return Ok(0);
        }
        // Held through the commit, so a concurrent `configure_vectors` cannot
        // change the dimension between the check and the write.
        let dim = self.dim.lock().unwrap();
        let Some(want) = *dim else {
            return Ok(0);
        };
        let mut stored = 0;
        let wtx = self.db.begin_write().map_err(redb_err)?;
        {
            let mut vecs = wtx.open_table(VECTORS).map_err(redb_err)?;
            for (doc_id, emb) in vectors {
                if emb.len() != want as usize {
                    tracing::warn!(
                        doc_id,
                        got = emb.len(),
                        want,
                        "embedding has the wrong dimension (the model changed?); not stored"
                    );
                    continue;
                }
                vecs.insert(vkey(*doc_id).as_slice(), vec_serialize(emb).as_slice())
                    .map_err(redb_err)?;
                stored += 1;
            }
        }
        wtx.commit().map_err(redb_err)?;
        Ok(stored)
    }

    /// Log a document that could not be embedded; it stays pending.
    fn warn_embed_failed(&self, doc_id: i64, err: &Error) -> Result<()> {
        let path = self.get_doc(doc_id)?.map(|r| r.path).unwrap_or_default();
        tracing::warn!(%path, error = %err, "embedding failed; the document stays pending");
        Ok(())
    }

    /// Re-index a document in tantivy (delete-then-add). Writes are buffered in
    /// the [`IndexWriter`]; call [`RetrieveStore::rebuild_fts`] to commit and
    /// make them searchable.
    fn reindex_fts(&self, doc_id: i64, text: &str) -> Result<()> {
        let w = self.writer.lock().unwrap();
        w.delete_term(Term::from_field_i64(self.fields.doc_id, doc_id));
        w.add_document(doc!(
            self.fields.doc_id => doc_id,
            self.fields.text => text.to_owned(),
        ))
        .map_err(tantivy_err)?;
        Ok(())
    }
}

impl RetrieveStore for RedbStore {
    fn upsert_document(&self, doc: &Document) -> Result<()> {
        // A new record, or one whose text changed, needs a new vector.
        let drop_vector = self
            .get_doc(doc.id)?
            .is_none_or(|prev| prev.text != doc.body);

        let record = DocRecord {
            path: doc.path.clone(),
            text: doc.body.clone(),
        };
        let bytes = serde_json::to_vec(&record).map_err(|e| Error::Redb(e.to_string()))?;

        let wtx = self.db.begin_write().map_err(redb_err)?;
        {
            let mut docs = wtx.open_table(DOCUMENTS).map_err(redb_err)?;
            docs.insert(doc.id, bytes.as_slice()).map_err(redb_err)?;
            if drop_vector {
                let mut vecs = wtx.open_table(VECTORS).map_err(redb_err)?;
                vecs.remove(vkey(doc.id).as_slice()).map_err(redb_err)?;
            }
        }
        wtx.commit().map_err(redb_err)?;

        self.reindex_fts(doc.id, &record.text)
    }

    fn remove_document(&self, id: i64) -> Result<()> {
        let wtx = self.db.begin_write().map_err(redb_err)?;
        {
            let mut docs = wtx.open_table(DOCUMENTS).map_err(redb_err)?;
            docs.remove(id).map_err(redb_err)?;
            let mut vecs = wtx.open_table(VECTORS).map_err(redb_err)?;
            vecs.remove(vkey(id).as_slice()).map_err(redb_err)?;
        }
        wtx.commit().map_err(redb_err)?;

        let w = self.writer.lock().unwrap();
        w.delete_term(Term::from_field_i64(self.fields.doc_id, id));
        Ok(())
    }

    fn rebuild_fts(&self) -> Result<()> {
        {
            let mut w = self.writer.lock().unwrap();
            w.commit().map_err(tantivy_err)?;
        }
        self.reader.reload().map_err(tantivy_err)?;
        Ok(())
    }

    fn document_ids(&self) -> Result<Vec<i64>> {
        let rtx = self.db.begin_read().map_err(redb_err)?;
        let t = rtx.open_table(DOCUMENTS).map_err(redb_err)?;
        let mut ids = Vec::new();
        for entry in t.iter().map_err(redb_err)? {
            let (k, _) = entry.map_err(redb_err)?;
            ids.push(k.value());
        }
        Ok(ids)
    }

    fn document_count(&self) -> Result<u64> {
        let rtx = self.db.begin_read().map_err(redb_err)?;
        let t = rtx.open_table(DOCUMENTS).map_err(redb_err)?;
        t.len().map_err(redb_err)
    }

    fn configure_vectors(&self, model: &str, dim: u32) -> Result<()> {
        // Held across the transaction, so two concurrent calls cannot interleave.
        let mut cur = self.dim.lock().unwrap();
        let wtx = self.db.begin_write().map_err(redb_err)?;
        let changed = {
            let mut meta = wtx.open_table(META).map_err(redb_err)?;
            let same_model = meta
                .get("embedding_model")
                .map_err(redb_err)?
                .is_some_and(|g| g.value() == model.as_bytes());
            let same_dim = meta
                .get("embedding_dim")
                .map_err(redb_err)?
                .is_some_and(|g| g.value() == dim.to_le_bytes().as_slice());
            let changed = !(same_model && same_dim);
            if changed {
                // Vectors from another model (or of another size) are not
                // comparable with the new ones: drop them all, so every
                // document becomes pending again.
                wtx.open_table(VECTORS)
                    .map_err(redb_err)?
                    .retain(|_, _| false)
                    .map_err(redb_err)?;
                meta.insert("embedding_model", model.as_bytes())
                    .map_err(redb_err)?;
                meta.insert("embedding_dim", dim.to_le_bytes().as_slice())
                    .map_err(redb_err)?;
            }
            changed
        };
        if changed {
            wtx.commit().map_err(redb_err)?;
        } else {
            wtx.abort().map_err(redb_err)?;
        }
        *cur = Some(dim);
        Ok(())
    }

    fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        source: &dyn VectorSource,
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<usize> {
        if self.dim().is_none() {
            return Ok(0);
        }

        // Collect (doc_id, path, text) for non-empty documents that lack a vector.
        let mut pending: Vec<(i64, String, String)> = Vec::new();
        {
            let rtx = self.db.begin_read().map_err(redb_err)?;
            let vecs = rtx.open_table(VECTORS).map_err(redb_err)?;
            let docs = rtx.open_table(DOCUMENTS).map_err(redb_err)?;
            for entry in docs.iter().map_err(redb_err)? {
                let (k, v) = entry.map_err(redb_err)?;
                let doc_id = k.value();
                let Ok(rec) = serde_json::from_slice::<DocRecord>(v.value()) else {
                    continue;
                };
                if rec.text.trim().is_empty() {
                    continue;
                }
                if vecs
                    .get(vkey(doc_id).as_slice())
                    .map_err(redb_err)?
                    .is_none()
                {
                    pending.push((doc_id, rec.path, rec.text));
                }
            }
        }

        // What the source already has is stored as it is; what it neither has nor allows
        // stays pending. Only the rest is computed.
        let mut found = Vec::new();
        let mut to_embed = Vec::new();
        for (doc_id, path, text) in pending {
            if let Some(v) = source.find(&path, &text) {
                found.push((doc_id, v));
            } else if source.may_embed(&path, &text) {
                to_embed.push((doc_id, path, text));
            }
        }
        let found = self.put_vectors(&found)?;
        let pending = to_embed;

        let total = pending.len();
        let mut done = 0;
        let mut embedded = found;
        let mut last_err = None;
        // Set when the loop broke on a fully-failed batch: the caller is told,
        // even if earlier batches embedded (#194).
        let mut broke_early = false;
        for batch in pending.chunks(100) {
            let texts: Vec<&str> = batch.iter().map(|(_, _, t)| t.as_str()).collect();
            let (vectors, outage): (Vec<(i64, Vec<f32>)>, bool) = match embedder.embed_texts(&texts)
            {
                Ok(embeddings) => (
                    batch.iter().map(|(id, _, _)| *id).zip(embeddings).collect(),
                    false,
                ),
                // One bad input must not block the rest: retry the batch one
                // document at a time, and leave each one that still fails pending.
                Err(_) => {
                    let mut ok = Vec::new();
                    for (doc_id, _, text) in batch {
                        match embedder.embed_texts(&[text.as_str()]) {
                            Ok(mut v) if v.len() == 1 => ok.push((*doc_id, v.remove(0))),
                            Ok(v) => {
                                let e = Error::Embed(format!(
                                    "embedder returned {} vectors for 1 input",
                                    v.len()
                                ));
                                self.warn_embed_failed(*doc_id, &e)?;
                                last_err = Some(e);
                            }
                            Err(e) => {
                                self.warn_embed_failed(*doc_id, &e)?;
                                last_err = Some(e);
                            }
                        }
                    }
                    // Not one input of the batch went through on its own: the
                    // provider is down, not one input bad.
                    let outage = ok.is_empty();
                    (ok, outage)
                }
            };
            embedded += self.put_vectors(&vectors)?;
            // Handed on only once the index holds them: a vector the index refused (a wrong
            // dimension) must not become a file other devices trust.
            let by_id: std::collections::HashMap<i64, (&str, &str)> = batch
                .iter()
                .map(|(id, p, t)| (*id, (p.as_str(), t.as_str())))
                .collect();
            let want = self.dim().unwrap_or(0) as usize;
            for (doc_id, v) in &vectors {
                if v.len() == want
                    && let Some((path, text)) = by_id.get(doc_id)
                {
                    source.embedded(path, text, v);
                }
            }
            done += batch.len();
            on_progress(done, total);
            if outage {
                // Stop rather than retry every remaining document one at a
                // time; they stay pending for the next run (#194). The batch
                // loop's `last_err` is the provider's own error, from the
                // per-item retries above.
                broke_early = true;
                break;
            }
        }
        // The loop broke on a fully-failed batch: a provider outage, whether
        // or not earlier batches embedded. The caller must hear it (#194), so
        // it is an error even when `embedded` is not zero. The vectors the
        // earlier batches stored are already committed; the rest stay pending.
        if broke_early {
            return Err(
                last_err.unwrap_or_else(|| Error::Embed("the embedding provider went down".into()))
            );
        }
        // Nothing embedded in this run and the loop did not break: report the
        // last failure, so a broken configuration is not silent.
        match last_err {
            Some(e) if embedded == 0 => Err(e),
            _ => Ok(embedded),
        }
    }

    fn vec_info(&self) -> Result<VecInfo> {
        let Some(dim) = self.dim() else {
            return Ok(VecInfo {
                embedding_dim: 0,
                vector_count: 0,
                pending_count: 0,
            });
        };
        let rtx = self.db.begin_read().map_err(redb_err)?;
        let vector_count = rtx
            .open_table(VECTORS)
            .map_err(redb_err)?
            .len()
            .map_err(redb_err)?;
        let mut embeddable: u64 = 0;
        let docs = rtx.open_table(DOCUMENTS).map_err(redb_err)?;
        for entry in docs.iter().map_err(redb_err)? {
            let (_, v) = entry.map_err(redb_err)?;
            if let Ok(rec) = serde_json::from_slice::<DocRecord>(v.value())
                && !rec.text.trim().is_empty()
            {
                embeddable += 1;
            }
        }
        Ok(VecInfo {
            embedding_dim: dim,
            vector_count,
            pending_count: embeddable.saturating_sub(vector_count),
        })
    }

    fn search_fts(&self, q: &FtsQuery<'_>) -> Result<Vec<FileSearchResult>> {
        self.reader.reload().map_err(tantivy_err)?;
        let searcher = self.reader.searcher();
        let mut qp = QueryParser::for_index(&self.index, vec![self.fields.text]);
        qp.set_conjunction_by_default();
        let query = match qp.parse_query(q.query) {
            Ok(query) => query,
            Err(_) => return Ok(Vec::new()), // unparseable / too-short query
        };
        let over_fetch = q.limit.saturating_mul(5).max(q.limit);
        let hits = searcher
            .search(&query, &TopDocs::with_limit(over_fetch).order_by_score())
            .map_err(tantivy_err)?;
        let generator =
            SnippetGenerator::create(&searcher, &*query, self.fields.text).map_err(tantivy_err)?;

        let prefix = q.path_prefix.map(|p| p.to_string_lossy().to_string());
        let mut seen: HashSet<i64> = HashSet::new();
        let mut results: Vec<FileSearchResult> = Vec::new();
        // BM25: higher = better; tantivy already returns hits in that order.
        for (score, addr) in hits {
            if results.len() == q.limit {
                break;
            }
            let d: TantivyDocument = searcher.doc(addr).map_err(tantivy_err)?;
            let Some(doc_id) = d.get_first(self.fields.doc_id).and_then(|v| v.as_i64()) else {
                continue;
            };
            if !seen.insert(doc_id) {
                continue;
            }
            let Some(rec) = self.get_doc(doc_id)? else {
                continue;
            };
            if let Some(pfx) = &prefix
                && !rec.path.starts_with(pfx.as_str())
            {
                continue;
            }
            let source = snippet_source(&rec.text, SNIPPET_SOURCE_BYTES);
            let snippet = pick_snippet(generator.snippet(source).fragment(), &rec.text);
            results.push(FileSearchResult {
                id: doc_id,
                path: rec.path,
                score: score as f64,
                snippet,
            });
        }
        Ok(results)
    }

    fn search_similar(&self, q: &VectorQuery<'_>) -> Result<Vec<FileSearchResult>> {
        if self.dim().is_none() {
            return Ok(Vec::new());
        }
        let query_vecs = q.embedder.embed_texts(&[q.query])?;
        let query_vec = query_vecs
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embed("embedder returned empty result".into()))?;

        let over_fetch = q.limit.saturating_mul(5).max(q.limit);

        // Brute-force scan: keep the best `over_fetch` by L2 distance.
        let mut scored: Vec<(f64, i64)> = Vec::new();
        {
            let rtx = self.db.begin_read().map_err(redb_err)?;
            let vecs = rtx.open_table(VECTORS).map_err(redb_err)?;
            for entry in vecs.iter().map_err(redb_err)? {
                let (k, v) = entry.map_err(redb_err)?;
                let Some(doc_id) = vkey_parse(k.value()) else {
                    continue;
                };
                let emb = vec_deserialize(v.value());
                if emb.len() != query_vec.len() {
                    continue;
                }
                scored.push((l2_distance(&query_vec, &emb), doc_id));
            }
        }
        // L2 distance: lower = better.
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(over_fetch);

        let prefix = q.path_prefix.map(|p| p.to_string_lossy().to_string());
        let mut results: Vec<FileSearchResult> = Vec::new();
        for (dist, doc_id) in scored {
            if results.len() == q.limit {
                break;
            }
            let Some(rec) = self.get_doc(doc_id)? else {
                continue;
            };
            if let Some(pfx) = &prefix
                && !rec.path.starts_with(pfx.as_str())
            {
                continue;
            }
            results.push(FileSearchResult {
                id: doc_id,
                path: rec.path,
                score: dist,
                snippet: leading(&rec.text),
            });
        }
        Ok(results)
    }
}

// ── maintenance ────────────────────────────────────────────────────────────────

/// Delete the on-disk store at `dir` (used by a full rebuild).
pub fn wipe_store(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Directory name (under the workspace cache dir) for a redb retrieve store.
pub fn store_dir(base: &Path) -> PathBuf {
    base.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieve_store::Document;

    /// Deterministic embedder: banana→[1,0,0], cherry→[0,1,0], else→[0,0,1].
    struct FakeEmbedder;
    impl Embedder for FakeEmbedder {
        fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.contains("banana") {
                        vec![1.0, 0.0, 0.0]
                    } else if t.contains("cherry") {
                        vec![0.0, 1.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    fn doc(id: i64, path: &str, text: &str) -> Document {
        Document {
            id,
            body: text.to_owned(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn fts_and_vector_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RedbStore::open(tmp.path(), Some(3)).unwrap();

        store
            .upsert_document(&doc(1, "/a.md", "the banana is yellow"))
            .unwrap();
        store
            .upsert_document(&doc(2, "/b.md", "a cherry is red"))
            .unwrap();
        store.rebuild_fts().unwrap();

        assert_eq!(store.document_count().unwrap(), 2);

        // Full-text search (trigram) finds the right document.
        let hits = store
            .search_fts(&FtsQuery::new("banana").limit(10))
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
        assert_eq!(hits[0].path, "/a.md");

        // Embed pending documents, then semantic search.
        let embedder = FakeEmbedder;
        let embedded = store
            .embed_pending(
                &embedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap();
        assert_eq!(embedded, 2);
        let info = store.vec_info().unwrap();
        assert_eq!(info.vector_count, 2);
        assert_eq!(info.pending_count, 0);

        let sem = store
            .search_similar(&VectorQuery::new("banana", &embedder).limit(10))
            .unwrap();
        assert_eq!(sem[0].id, 1, "closest vector should be the banana doc");

        // Removal drops the document from both stores.
        store.remove_document(1).unwrap();
        store.rebuild_fts().unwrap();
        assert_eq!(store.document_count().unwrap(), 1);
        let hits = store
            .search_fts(&FtsQuery::new("banana").limit(10))
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn persists_and_reopens_with_dim() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let store = RedbStore::open(tmp.path(), Some(3)).unwrap();
            store
                .upsert_document(&doc(1, "/a.md", "hello world text"))
                .unwrap();
            store.rebuild_fts().unwrap();
        }
        // Reopen without passing dim: it is recovered from meta.
        let store = RedbStore::open(tmp.path(), None).unwrap();
        assert_eq!(store.dim(), Some(3));
        assert_eq!(store.document_count().unwrap(), 1);
        let hits = store.search_fts(&FtsQuery::new("world").limit(10)).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn one_result_per_file_with_an_fts_snippet() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        let body = "first paragraph about apples\n\nsecond paragraph about bananas\n\nthird about apples again";
        store.upsert_document(&doc(1, "/w/a.md", body)).unwrap();
        store
            .upsert_document(&doc(2, "/w/b.md", "nothing relevant here"))
            .unwrap();
        store.rebuild_fts().unwrap();

        let hits = store
            .search_fts(&FtsQuery::new("bananas").limit(10))
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/w/a.md");
        assert!(hits[0].snippet.contains("bananas"), "{:?}", hits[0].snippet);
        assert!(hits[0].snippet.chars().count() <= crate::snippet::SNIPPET_CHARS);
        assert!(!hits[0].snippet.contains('\n'));
    }

    #[test]
    fn vector_hits_carry_the_leading_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store
            .upsert_document(&doc(1, "/w/a.md", "alpha beta gamma"))
            .unwrap();
        store.rebuild_fts().unwrap();
        assert_eq!(
            store
                .embed_pending(
                    &FakeEmbedder,
                    &crate::retrieve_store::NoVectorSource,
                    &|_, _| {}
                )
                .unwrap(),
            1
        );

        let hits = store
            .search_similar(&VectorQuery::new("alpha", &FakeEmbedder).limit(5))
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "alpha beta gamma");
        let info = store.vec_info().unwrap();
        assert_eq!((info.vector_count, info.pending_count), (1, 0));
    }

    #[test]
    fn a_changed_body_drops_the_vector_and_an_unchanged_one_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "one")).unwrap();
        store
            .embed_pending(
                &FakeEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap();

        store.upsert_document(&doc(1, "/w/a.md", "one")).unwrap();
        assert_eq!(store.vec_info().unwrap().pending_count, 0);

        store.upsert_document(&doc(1, "/w/a.md", "two")).unwrap();
        assert_eq!(store.vec_info().unwrap().pending_count, 1);
    }

    #[test]
    fn an_empty_file_is_indexed_but_never_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store.upsert_document(&doc(1, "/w/empty.md", "")).unwrap();
        assert_eq!(store.document_count().unwrap(), 1);
        assert_eq!(
            store
                .embed_pending(
                    &FakeEmbedder,
                    &crate::retrieve_store::NoVectorSource,
                    &|_, _| {}
                )
                .unwrap(),
            0
        );
        assert_eq!(store.vec_info().unwrap().pending_count, 0);
    }

    #[test]
    fn path_prefix_filters_fts_and_vector_results() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store
            .upsert_document(&doc(1, "/w/x/a.md", "shared words"))
            .unwrap();
        store
            .upsert_document(&doc(2, "/w/y/b.md", "shared words"))
            .unwrap();
        store.rebuild_fts().unwrap();
        store
            .embed_pending(
                &FakeEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap();
        let x = std::path::Path::new("/w/x");

        let fts = store
            .search_fts(&FtsQuery::new("shared").path_prefix(x).limit(10))
            .unwrap();
        let vec = store
            .search_similar(
                &VectorQuery::new("shared", &FakeEmbedder)
                    .path_prefix(x)
                    .limit(10),
            )
            .unwrap();

        assert_eq!(fts.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(vec.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn a_store_from_the_previous_schema_is_wiped_once() {
        let dir = tempfile::tempdir().unwrap();
        {
            // Simulate the previous build: a populated store with no schema_version.
            let store = RedbStore::open(dir.path(), None).unwrap();
            store.upsert_document(&doc(1, "/w/a.md", "old")).unwrap();
            store.rebuild_fts().unwrap();
            store.clear_meta_for_test("schema_version");
        }
        let reopened = RedbStore::open(dir.path(), None).unwrap();
        assert_eq!(
            reopened.document_count().unwrap(),
            0,
            "the old store is wiped"
        );
        reopened.upsert_document(&doc(2, "/w/b.md", "new")).unwrap();
        drop(reopened);

        let again = RedbStore::open(dir.path(), None).unwrap();
        assert_eq!(
            again.document_count().unwrap(),
            1,
            "a current store is not wiped again"
        );
    }

    #[test]
    fn pick_snippet_uses_the_fragment_else_the_leading_text() {
        assert_eq!(pick_snippet("", "leading  text\nhere"), "leading text here");
        assert_eq!(pick_snippet("   ", "leading text"), "leading text");
        assert_eq!(
            pick_snippet("about\n\nbananas", "leading text"),
            "about bananas"
        );
    }

    #[test]
    fn an_empty_store_from_the_previous_schema_still_opens() {
        // The previous build created its tantivy index (with a `line_start`
        // field) on first open, even when no document was ever indexed.
        let dir = tempfile::tempdir().unwrap();
        let tantivy_dir = dir.path().join("tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        let mut sb = Schema::builder();
        sb.add_i64_field("doc_id", INDEXED | STORED);
        sb.add_u64_field("line_start", STORED);
        sb.add_text_field("text", TextOptions::default());
        Index::create_in_dir(&tantivy_dir, sb.build()).unwrap();

        let store = RedbStore::open(dir.path(), None).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "hello")).unwrap();
        store.rebuild_fts().unwrap();
        assert_eq!(
            store
                .search_fts(&FtsQuery::new("hello").limit(5))
                .unwrap()
                .len(),
            1
        );
    }

    /// Fails any call whose input contains `POISON`; otherwise embeds like
    /// [`FakeEmbedder`].
    struct PoisonEmbedder;
    impl Embedder for PoisonEmbedder {
        fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if texts.iter().any(|t| t.contains("POISON")) {
                return Err(Error::Embed("input rejected".into()));
            }
            FakeEmbedder.embed_texts(texts)
        }
    }

    struct BrokenEmbedder;
    impl Embedder for BrokenEmbedder {
        fn embed_texts(&self, _: &[&str]) -> Result<Vec<Vec<f32>>> {
            Err(Error::Embed("provider unreachable".into()))
        }
    }

    #[test]
    fn a_failing_document_stays_pending_and_the_rest_are_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "banana")).unwrap();
        store
            .upsert_document(&doc(2, "/w/bad.md", "POISON pill"))
            .unwrap();
        store.upsert_document(&doc(3, "/w/c.md", "cherry")).unwrap();

        let embedded = store
            .embed_pending(
                &PoisonEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap();

        assert_eq!(embedded, 2);
        let info = store.vec_info().unwrap();
        assert_eq!((info.vector_count, info.pending_count), (2, 1));
        // The failure is not sticky: once the input is fixed it embeds.
        store
            .upsert_document(&doc(2, "/w/bad.md", "fixed"))
            .unwrap();
        assert_eq!(
            store
                .embed_pending(
                    &PoisonEmbedder,
                    &crate::retrieve_store::NoVectorSource,
                    &|_, _| {}
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn an_embedder_that_fails_every_call_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "one")).unwrap();
        store.upsert_document(&doc(2, "/w/b.md", "two")).unwrap();

        let err = store
            .embed_pending(
                &BrokenEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap_err();

        assert!(err.to_string().contains("provider unreachable"), "{err}");
        assert_eq!(store.vec_info().unwrap().pending_count, 2);
    }

    #[test]
    fn configure_vectors_on_a_new_model_drops_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        store.configure_vectors("m", 3).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "banana")).unwrap();
        store.upsert_document(&doc(2, "/w/b.md", "cherry")).unwrap();
        assert_eq!(
            store
                .embed_pending(
                    &FakeEmbedder,
                    &crate::retrieve_store::NoVectorSource,
                    &|_, _| {}
                )
                .unwrap(),
            2
        );

        store.configure_vectors("other", 3).unwrap();

        let info = store.vec_info().unwrap();
        assert_eq!(info.vector_count, 0);
        assert!(info.pending_count > 0);
        assert_eq!(info.embedding_dim, 3);
    }

    #[test]
    fn configure_vectors_with_a_new_dim_drops_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        store.configure_vectors("m", 3).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "banana")).unwrap();
        store
            .embed_pending(
                &FakeEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap();

        store.configure_vectors("m", 4).unwrap();

        let info = store.vec_info().unwrap();
        assert_eq!((info.embedding_dim, info.vector_count), (4, 0));
        assert_eq!(store.dim(), Some(4));
    }

    #[test]
    fn configure_vectors_with_the_same_model_keeps_vectors() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = RedbStore::open(dir.path(), None).unwrap();
            store.configure_vectors("m", 3).unwrap();
            store.upsert_document(&doc(1, "/w/a.md", "banana")).unwrap();
            store.upsert_document(&doc(2, "/w/b.md", "cherry")).unwrap();
            store
                .embed_pending(
                    &FakeEmbedder,
                    &crate::retrieve_store::NoVectorSource,
                    &|_, _| {},
                )
                .unwrap();
            store.configure_vectors("m", 3).unwrap();
            assert_eq!(store.vec_info().unwrap().vector_count, 2);
        }
        // Across a reopen too: the model and the dim are kept in meta.
        let store = RedbStore::open(dir.path(), None).unwrap();
        store.configure_vectors("m", 3).unwrap();
        let info = store.vec_info().unwrap();
        assert_eq!((info.vector_count, info.pending_count), (2, 0));
    }

    #[test]
    fn configure_vectors_enables_vectors_on_a_store_opened_without_a_dim() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        assert_eq!(store.dim(), None);
        store.upsert_document(&doc(1, "/w/a.md", "banana")).unwrap();
        store.upsert_document(&doc(2, "/w/b.md", "cherry")).unwrap();

        store.configure_vectors("m", 3).unwrap();

        assert_eq!(store.dim(), Some(3));
        assert_eq!(
            store
                .embed_pending(
                    &FakeEmbedder,
                    &crate::retrieve_store::NoVectorSource,
                    &|_, _| {}
                )
                .unwrap(),
            2
        );
        let hits = store
            .search_similar(&VectorQuery::new("cherry", &FakeEmbedder).limit(5))
            .unwrap();
        assert_eq!(hits[0].id, 2);
    }

    /// Fails every call and counts them.
    struct CountingBrokenEmbedder(std::sync::atomic::AtomicUsize);
    impl Embedder for CountingBrokenEmbedder {
        fn embed_texts(&self, _: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(Error::Embed("provider unreachable".into()))
        }
    }

    #[test]
    fn embed_pending_stops_after_a_fully_failed_batch() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        for i in 0..150 {
            store
                .upsert_document(&doc(i, &format!("/w/{i}.md"), &format!("text {i}")))
                .unwrap();
        }
        let embedder = CountingBrokenEmbedder(Default::default());

        let err = store
            .embed_pending(
                &embedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap_err();

        assert!(err.to_string().contains("provider unreachable"), "{err}");
        // One call for batch 1, then 100 single retries; batch 2 is never tried.
        assert_eq!(
            embedder.0.load(std::sync::atomic::Ordering::SeqCst),
            1 + 100
        );
        assert_eq!(store.vec_info().unwrap().pending_count, 150);
    }

    #[test]
    fn a_fully_failed_last_batch_keeps_the_earlier_progress() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        // 101 documents: batch 1 is fine, batch 2 is one bad input.
        for i in 0..101 {
            let body = if i == 100 {
                "POISON pill".to_owned()
            } else {
                format!("text {i}")
            };
            store
                .upsert_document(&doc(i, &format!("/w/{i}.md"), &body))
                .unwrap();
        }

        // The last batch failed as a whole: the call reports it (#194), even though the
        // first batch embedded.
        let err = store
            .embed_pending(
                &PoisonEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap_err();
        assert!(err.to_string().contains("input rejected"), "{err}");

        // The 100 documents the first batch embedded are stored, not re-pended; only the
        // one bad input stays pending.
        let info = store.vec_info().unwrap();
        assert_eq!((info.vector_count, info.pending_count), (100, 1));
    }

    #[test]
    fn a_mid_run_outage_is_reported_and_leaves_the_later_batch_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        // 150 documents over 2 batches. Every document of batch 2 carries "POISON", so the
        // provider fails that whole batch — and every per-item retry — while batch 1's
        // documents all embed.
        for i in 0..150 {
            let body = if i >= 100 {
                "POISON pill".to_owned()
            } else {
                format!("text {i}")
            };
            store
                .upsert_document(&doc(i, &format!("/w/{i}.md"), &body))
                .unwrap();
        }

        let err = store
            .embed_pending(
                &PoisonEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap_err();
        assert!(err.to_string().contains("input rejected"), "{err}");

        // Batch 1's 100 documents are embedded; batch 2's 50 stay pending for the next run.
        let info = store.vec_info().unwrap();
        assert_eq!((info.vector_count, info.pending_count), (100, 50));
    }

    #[test]
    fn vectors_of_the_wrong_size_are_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        store.configure_vectors("old", 3).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "banana")).unwrap();
        store.upsert_document(&doc(2, "/w/b.md", "cherry")).unwrap();
        // A run that started under the old model finishes after the switch.
        store.configure_vectors("new", 4).unwrap();

        let stored = store
            .put_vectors(&[(1, vec![1.0, 0.0, 0.0]), (2, vec![0.0, 1.0, 0.0, 0.0])])
            .unwrap();

        assert_eq!(stored, 1);
        let info = store.vec_info().unwrap();
        assert_eq!((info.vector_count, info.pending_count), (1, 1));
        // Through embed_pending too: a 3-value embedder stores nothing in a 4-dim store.
        assert_eq!(
            store
                .embed_pending(
                    &FakeEmbedder,
                    &crate::retrieve_store::NoVectorSource,
                    &|_, _| {}
                )
                .unwrap(),
            0
        );
        assert_eq!(store.vec_info().unwrap().vector_count, 1);
    }

    #[test]
    fn a_bad_item_in_a_working_batch_does_not_stop_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        // 150 documents over 2 batches; the first batch holds one bad input.
        for i in 0..150 {
            let body = if i == 0 {
                "POISON pill".to_owned()
            } else {
                format!("text {i}")
            };
            store
                .upsert_document(&doc(i, &format!("/w/{i}.md"), &body))
                .unwrap();
        }

        let embedded = store
            .embed_pending(
                &PoisonEmbedder,
                &crate::retrieve_store::NoVectorSource,
                &|_, _| {},
            )
            .unwrap();

        assert_eq!(embedded, 149);
        let info = store.vec_info().unwrap();
        assert_eq!((info.vector_count, info.pending_count), (149, 1));
    }

    #[test]
    fn a_japanese_match_gets_a_short_one_line_snippet() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        let body = format!(
            "{}\n\nここに検索対象のテキストがあります。\n\n{}",
            "日本語の文章が続きます。".repeat(20),
            "最後の段落です。".repeat(10)
        );
        assert!(body.chars().count() > 150);
        store.upsert_document(&doc(1, "/w/ja.md", &body)).unwrap();
        store.rebuild_fts().unwrap();

        let hits = store
            .search_fts(&FtsQuery::new("テキスト").limit(5))
            .unwrap();

        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(
            snippet.chars().count() <= crate::snippet::SNIPPET_CHARS,
            "{snippet:?}"
        );
        assert!(!snippet.contains('\n'), "{snippet:?}");
        assert!(snippet.contains("テキスト"), "{snippet:?}");
    }

    #[test]
    fn a_match_past_the_snippet_window_falls_back_to_the_leading_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        let body = format!("start {} needle", "x ".repeat(SNIPPET_SOURCE_BYTES));
        store.upsert_document(&doc(1, "/w/big.md", &body)).unwrap();
        store.rebuild_fts().unwrap();

        let hits = store.search_fts(&FtsQuery::new("needle").limit(5)).unwrap();

        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.starts_with("start x"),
            "{:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn the_snippet_source_is_cut_on_a_char_boundary() {
        let s = "あいう"; // 3 bytes per char
        assert_eq!(snippet_source(s, 4), "あ");
        assert_eq!(snippet_source(s, 6), "あい");
        assert_eq!(snippet_source(s, 100), s);
    }

    #[test]
    fn a_match_deep_in_a_long_file_still_gets_a_snippet() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        let body = format!("{} needle", "x ".repeat(2_000));
        store.upsert_document(&doc(1, "/w/a.md", &body)).unwrap();
        store.rebuild_fts().unwrap();

        let hits = store.search_fts(&FtsQuery::new("needle").limit(5)).unwrap();

        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.is_empty());
    }

    /// Finds the vectors in `stored` by path, allows embedding the paths in `allowed`, and
    /// records what it is handed.
    struct MapSource {
        stored: std::collections::HashMap<String, Vec<f32>>,
        allowed: Vec<String>,
        kept: std::sync::Mutex<Vec<(String, Vec<f32>)>>,
    }

    impl crate::retrieve_store::VectorSource for MapSource {
        fn find(&self, path: &str, _: &str) -> Option<Vec<f32>> {
            self.stored.get(path).cloned()
        }
        fn may_embed(&self, path: &str, _: &str) -> bool {
            self.allowed.iter().any(|p| p == path)
        }
        fn embedded(&self, path: &str, _: &str, vector: &[f32]) {
            self.kept
                .lock()
                .unwrap()
                .push((path.to_owned(), vector.to_vec()));
        }
    }

    /// Counts the texts it embeds.
    struct CountingEmbedder(std::sync::atomic::AtomicUsize);
    impl Embedder for CountingEmbedder {
        fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.0
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            FakeEmbedder.embed_texts(texts)
        }
    }

    #[test]
    fn a_source_supplies_what_it_has_and_decides_what_is_computed() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store
            .upsert_document(&doc(1, "/w/found.md", "banana"))
            .unwrap();
        store
            .upsert_document(&doc(2, "/w/mine.md", "cherry"))
            .unwrap();
        store
            .upsert_document(&doc(3, "/w/theirs.md", "other"))
            .unwrap();
        store.rebuild_fts().unwrap();
        let source = MapSource {
            stored: [("/w/found.md".to_owned(), vec![0.5, 0.5, 0.0])].into(),
            allowed: vec!["/w/mine.md".to_owned()],
            kept: Default::default(),
        };
        let embedder = CountingEmbedder(Default::default());

        let got = store.embed_pending(&embedder, &source, &|_, _| {}).unwrap();

        assert_eq!(got, 2, "one found, one computed");
        assert_eq!(embedder.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            *source.kept.lock().unwrap(),
            vec![("/w/mine.md".to_owned(), vec![0.0, 1.0, 0.0])],
            "only the computed vector is handed back"
        );
        let info = store.vec_info().unwrap();
        assert_eq!(
            (info.vector_count, info.pending_count),
            (2, 1),
            "the refused one stays pending"
        );
        // The found vector is what search uses.
        let hits = store
            .search_similar(&VectorQuery::new("banana", &FakeEmbedder).limit(1))
            .unwrap();
        assert!(hits[0].path.ends_with("found.md") || hits[0].path.ends_with("mine.md"));
    }

    #[test]
    fn a_found_vector_of_the_wrong_dimension_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store.upsert_document(&doc(1, "/w/a.md", "banana")).unwrap();
        let source = MapSource {
            stored: [("/w/a.md".to_owned(), vec![1.0, 0.0])].into(),
            allowed: vec![],
            kept: Default::default(),
        };
        assert_eq!(
            store
                .embed_pending(&FakeEmbedder, &source, &|_, _| {})
                .unwrap(),
            0
        );
        assert_eq!(store.vec_info().unwrap().pending_count, 1);
    }
}
