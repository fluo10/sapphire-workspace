# Remove the Chunker Implementation Plan (#184)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Retrieval indexes and embeds every file as one document with one vector. Search results carry a snippet in place of chunk lists.

**Architecture:** The `chunker` module and the chunk-level types are deleted from `sapphire-framework-retrieve`. `RedbStore` keeps one record per file (`{path, text}`), one tantivy document per file, and one vector per file keyed by `doc_id`. A `schema_version` meta key makes an old-shape store wipe and rebuild itself. A new pure `snippet` module builds result excerpts: tantivy `SnippetGenerator` for FTS hits, the leading text otherwise. The workspace indexer stops branching on extension. The backend protocol version goes 2 → 3.

**Tech Stack:** Rust 2024, redb 4, tantivy 0.26, serde_json.

**Spec:** `docs/superpowers/specs/2026-10-08-remove-chunker-design.md`

## Global Constraints

- `pub const SNIPPET_CHARS: usize = 150;`. Snippets are cut on `char` boundaries, whitespace runs (including newlines) collapse to one space, and the result is trimmed.
- `const MAX_REST_EMBED_CHARS: usize = 8_000;`. It applies to the OpenAI-compatible and Ollama inputs, cut on a `char` boundary.
- Store `meta` key `schema_version`, current value `2` (u32 LE, via the existing `set_meta_u32` / `get_meta_u32`).
- `FileSearchResult { id: i64, path: String, score: f64, snippet: String }`. Score conventions are unchanged: FTS BM25 higher is better, vector L2 lower is better, hybrid RRF higher is better.
- `Document { id: i64, body: String, path: String }`.
- Backend `API_VERSION` goes from `2` to `3`.
- **Tests on this Windows host:** the default `fastembed-embed` feature links a static ONNX Runtime that crashes on this CPU (no AVX). Run retrieve and workspace tests with `--no-default-features --features redb-store`. Server, backend and the other crates run normally with `-p`. Do not run a workspace-wide `cargo test`.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Multibyte text at the snippet and cap boundaries.** No panic, valid UTF-8. Pinned by `snippet` tests with Japanese text longer than the cap (Task 1).
2. **A store written by the previous build.** It must be wiped once and then work. Reopening a v2 store must not wipe it again. Pinned in Task 2.
3. **An FTS query whose snippet generator yields nothing** (for example a 1–2 character query, which trigram FTS cannot match, or a match outside the generator's fragment window). The result must fall back to the leading text and never be empty for a non-empty body. Pinned in Task 2.
4. **An empty file.** It is indexed, the snippet is empty, and the file is never embedded with an empty-string panic. Pinned in Task 2.
5. **`path_prefix` filtering** still works for FTS and vector search. Pinned in Task 2.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/sapphire-framework-retrieve/src/snippet.rs` | **create** | `SNIPPET_CHARS`, `leading(text)`, `collapse_and_cut(text)` |
| `crates/sapphire-framework-retrieve/src/embed.rs` | modify | `MAX_REST_EMBED_CHARS`, `cap_chars()`, applied in `embed_openai` / `embed_ollama` |
| `crates/sapphire-framework-retrieve/src/chunker.rs` | **delete** | — |
| `crates/sapphire-framework-retrieve/src/retrieve_store.rs` | modify | `Document`, `FileSearchResult`; remove `ChunkHit` |
| `crates/sapphire-framework-retrieve/src/vector_store.rs` | modify | remove `Chunk`, `ChunkRow`, `group_by_file`; keep `VecInfo` + vector helpers |
| `crates/sapphire-framework-retrieve/src/redb_store.rs` | modify | new record shape, schema version, per-file tantivy and vectors, snippets |
| `crates/sapphire-framework-retrieve/src/db.rs` | modify | `InMemoryStore` snippets, `merge_rrf_files` without chunk merging |
| `crates/sapphire-framework-retrieve/src/lib.rs` | modify | module and export list |
| `crates/sapphire-framework-workspace/src/{indexer.rs,workspace_state.rs,lib.rs}` | modify | whole-file documents, re-exports |
| `crates/sapphire-framework-backend/src/protocol.rs` | modify | `API_VERSION = 3` |
| any other compile fallout | modify | adapt to `snippet` |
| `CHANGELOG.md`, `docs/ARCHITECTURE.md` | modify | Breaking note, retrieve section |

---

### Task 1: snippet helpers and the REST input cap (pure functions)

**Files:**
- Create: `crates/sapphire-framework-retrieve/src/snippet.rs`
- Modify: `crates/sapphire-framework-retrieve/src/embed.rs`
- Modify: `crates/sapphire-framework-retrieve/src/lib.rs` (add `pub mod snippet;` and `pub use snippet::SNIPPET_CHARS;`)

**Interfaces:**
- Produces:
  - `pub const SNIPPET_CHARS: usize = 150;`
  - `pub fn collapse_and_cut(text: &str) -> String`, which collapses whitespace runs to one space, trims, and keeps at most `SNIPPET_CHARS` chars.
  - `pub fn leading(text: &str) -> String`, which is `collapse_and_cut(text)` (named for intent at call sites).
  - `pub(crate) const MAX_REST_EMBED_CHARS: usize = 8_000;`
  - `pub(crate) fn cap_chars(text: &str, max: usize) -> &str`

- [ ] **Step 1: Write the failing tests** (bottom of `snippet.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_collapses_and_trims() {
        assert_eq!(collapse_and_cut("  a\n\n b\t c  "), "a b c");
    }

    #[test]
    fn short_text_is_kept_whole() {
        assert_eq!(leading("hello world"), "hello world");
    }

    #[test]
    fn long_text_is_cut_to_the_cap_in_chars() {
        let s = "あ".repeat(SNIPPET_CHARS + 50);
        let out = leading(&s);
        assert_eq!(out.chars().count(), SNIPPET_CHARS);
        assert!(out.chars().all(|c| c == 'あ'));
    }

    #[test]
    fn empty_text_gives_an_empty_snippet() {
        assert_eq!(leading(""), "");
        assert_eq!(leading(" \n\t "), "");
    }
}
```
In `embed.rs`, add a test module (or extend the existing one):
```rust
#[cfg(test)]
mod cap_tests {
    use super::*;

    #[test]
    fn cap_keeps_short_input() {
        assert_eq!(cap_chars("abc", 8), "abc");
    }

    #[test]
    fn cap_cuts_on_a_char_boundary() {
        let s = "日本語のテキスト";
        assert_eq!(cap_chars(s, 3), "日本語");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p sapphire-framework-retrieve --no-default-features --features redb-store snippet cap_`
Expected: compile errors, because the functions do not exist yet.

- [ ] **Step 3: Implement**

`snippet.rs`:
```rust
//! Short excerpts of a file for search results.
//!
//! A result names one whole file, so it carries a snippet instead of matched chunks:
//! the FTS fragment around the match when there is one, else the file's leading text.

/// Maximum length of a snippet, in characters.
pub const SNIPPET_CHARS: usize = 150;

/// Collapse whitespace runs (newlines included) to single spaces, trim, and keep at
/// most [`SNIPPET_CHARS`] characters. Cuts on `char` boundaries, so any UTF-8 text is safe.
pub fn collapse_and_cut(text: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            if count + 1 >= SNIPPET_CHARS {
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= SNIPPET_CHARS {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// The leading text of a file, as a snippet.
pub fn leading(text: &str) -> String {
    collapse_and_cut(text)
}
```
`embed.rs`, near the REST section:
```rust
/// Input cap for REST embedders, in characters. Deliberately conservative for CJK
/// text (about one token per character). A safety net until #185 truncates by tokens.
pub(crate) const MAX_REST_EMBED_CHARS: usize = 8_000;

/// `text` cut to at most `max` characters, on a `char` boundary.
pub(crate) fn cap_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}
```
In `embed_openai` and `embed_ollama`, build the request input from capped texts:
```rust
    let capped: Vec<&str> = texts.iter().map(|t| cap_chars(t, MAX_REST_EMBED_CHARS)).collect();
```
Send `capped` where `texts` was sent, in the `"input"` JSON field. Keep `texts.len()` as the expected count.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sapphire-framework-retrieve --no-default-features --features redb-store snippet cap_`
Expected: 6 pass.

- [ ] **Step 5: Commit**

```bash
git add crates/sapphire-framework-retrieve
git commit -m "feat(retrieve): snippet helpers and a character cap for REST embedder input" -m "Refs #184." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: one document per file in `sapphire-framework-retrieve`

**Files:**
- Delete: `crates/sapphire-framework-retrieve/src/chunker.rs`
- Modify: `retrieve_store.rs`, `vector_store.rs`, `redb_store.rs`, `db.rs`, `lib.rs` in `crates/sapphire-framework-retrieve/src/`

**Interfaces:**
- Consumes: `snippet::{collapse_and_cut, leading}` (Task 1).
- Produces: the types in the Global Constraints. `RedbStore` gains no new public API. `merge_rrf_files` keeps its signature.

- [ ] **Step 1: Write the failing tests** (replace the chunk-based tests in `redb_store.rs`'s test module; keep its existing `doc()` helper and test embedder, adapting `doc()` to the new `Document`)

```rust
    fn doc(id: i64, path: &str, text: &str) -> Document {
        Document { id, body: text.to_owned(), path: path.to_owned() }
    }

    #[test]
    fn one_result_per_file_with_an_fts_snippet() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        let body = "first paragraph about apples\n\nsecond paragraph about bananas\n\nthird about apples again";
        store.upsert_document(&doc(1, "/w/a.md", body)).unwrap();
        store.upsert_document(&doc(2, "/w/b.md", "nothing relevant here")).unwrap();
        store.rebuild_fts().unwrap();

        let hits = store.search_fts(&FtsQuery::new("bananas").limit(10)).unwrap();

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
        store.upsert_document(&doc(1, "/w/a.md", "alpha beta gamma")).unwrap();
        store.rebuild_fts().unwrap();
        assert_eq!(store.embed_pending(&FakeEmbedder, &|_, _| {}).unwrap(), 1);

        let hits = store.search_similar(&VectorQuery::new("alpha", &FakeEmbedder).limit(5)).unwrap();

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
        store.embed_pending(&FakeEmbedder, &|_, _| {}).unwrap();

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
        assert_eq!(store.embed_pending(&FakeEmbedder, &|_, _| {}).unwrap(), 0);
        assert_eq!(store.vec_info().unwrap().pending_count, 0);
    }

    #[test]
    fn path_prefix_filters_fts_and_vector_results() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), Some(3)).unwrap();
        store.upsert_document(&doc(1, "/w/x/a.md", "shared words")).unwrap();
        store.upsert_document(&doc(2, "/w/y/b.md", "shared words")).unwrap();
        store.rebuild_fts().unwrap();
        store.embed_pending(&FakeEmbedder, &|_, _| {}).unwrap();
        let x = std::path::Path::new("/w/x");

        let fts = store.search_fts(&FtsQuery::new("shared").path_prefix(x).limit(10)).unwrap();
        let vec = store.search_similar(&VectorQuery::new("shared", &FakeEmbedder).path_prefix(x).limit(10)).unwrap();

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
        assert_eq!(reopened.document_count().unwrap(), 0, "the old store is wiped");
        reopened.upsert_document(&doc(2, "/w/b.md", "new")).unwrap();
        drop(reopened);

        let again = RedbStore::open(dir.path(), None).unwrap();
        assert_eq!(again.document_count().unwrap(), 1, "a current store is not wiped again");
    }

    #[test]
    fn a_snippetless_fts_match_falls_back_to_the_leading_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path(), None).unwrap();
        let body = format!("{} needle", "x ".repeat(2_000));
        store.upsert_document(&doc(1, "/w/a.md", &body)).unwrap();
        store.rebuild_fts().unwrap();

        let hits = store.search_fts(&FtsQuery::new("needle").limit(5)).unwrap();

        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.is_empty());
    }
```
Use the test-embedder and dimension constant names that already exist in this module. If they differ from `FakeEmbedder` / `TEST_DIM`, use the existing ones. Add the test-only helper on `RedbStore`:
```rust
    #[cfg(test)]
    fn clear_meta_for_test(&self, key: &str) {
        let wtx = self.db.begin_write().unwrap();
        { wtx.open_table(META).unwrap().remove(key).unwrap(); }
        wtx.commit().unwrap();
    }
```
In `db.rs`'s tests (create a module if there is none), add:
```rust
    #[test]
    fn hybrid_merge_keeps_the_fts_snippet_and_sums_scores() {
        let f = |id, snip: &str, score| FileSearchResult { id, path: format!("/w/{id}.md"), score, snippet: snip.into() };
        let merged = merge_rrf_files(&[f(1, "fts", 1.0)], &[f(1, "lead", 0.5), f(2, "lead2", 0.7)], 60.0, 0.5, 0.5, 10);
        assert_eq!(merged[0].id, 1);
        assert_eq!(merged[0].snippet, "fts");
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn in_memory_fts_returns_a_snippet() {
        let store = InMemoryStore::new();
        store.upsert_document(&Document { id: 1, body: "hello world".into(), path: "/w/a.md".into() }).unwrap();
        let hits = store.search_fts(&FtsQuery::new("world").limit(5)).unwrap();
        assert_eq!(hits[0].snippet, "hello world");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p sapphire-framework-retrieve --no-default-features --features redb-store`
Expected: compile errors (`snippet` field, `Document` shape).

- [ ] **Step 3: Implement the types** (`retrieve_store.rs`)

Replace `Document`, delete `ChunkHit`, and replace `FileSearchResult` with:
```rust
/// A document to index for FTS and vector search: one file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Document {
    /// Stable identifier assigned by the caller.
    pub id: i64,
    /// The text that is indexed and embedded: the file's content.
    pub body: String,
    /// Absolute file path (shown in search results).
    pub path: String,
}

/// One file matching a search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileSearchResult {
    pub id: i64,
    pub path: String,
    /// FTS: BM25 (higher is better). Vector: L2 distance (lower is better).
    /// Hybrid: RRF (higher is better).
    pub score: f64,
    /// A short excerpt: the FTS fragment around the match, else the leading text.
    /// At most [`SNIPPET_CHARS`](crate::snippet::SNIPPET_CHARS) characters, on one line.
    pub snippet: String,
}
```
Update the trait doc comments that say "chunk granularity" to say "file granularity". In `embed_pending`'s doc, say "for all documents without a vector".

- [ ] **Step 4: Implement `vector_store.rs`**

Delete `Chunk`, `ChunkRow` and `group_by_file`, along with their imports. Keep `VecInfo`, and change its doc comments from "chunks" to "documents". Keep `vec_serialize`, `vec_deserialize` and `l2_distance`.

- [ ] **Step 5: Implement `redb_store.rs`**

1. Imports: drop `chunker::chunk_document`, `ChunkRow` and `group_by_file`. Add `crate::snippet::{collapse_and_cut, leading}` and `tantivy::snippet::SnippetGenerator`.
2. Records and keys:
```rust
/// `doc_id -> serde_json(DocRecord)`.
const DOCUMENTS: TableDefinition<i64, &[u8]> = TableDefinition::new("documents");
/// `doc_id (i64 LE) -> little-endian f32 blob`.
const VECTORS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("vectors");

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
```
   Delete `StoredChunk`. Update the module doc table and bullets to describe the per-file layout.
3. Schema: remove the `line_start` field from `Fields` and `build_schema`.
4. In `open`, right after `create_or_reset(dir)?` and the table creation, check the schema version. Use free-function equivalents of the meta helpers that operate on `&Database`, because `self` does not exist yet:
```rust
        let db = match read_meta_u32(&db, "schema_version")? {
            Some(SCHEMA_VERSION) => db,
            _ if is_empty(&db)? => db,
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
```
   Factor out `create_tables(&Database)`, `read_meta_u32(&Database, &str)`, `write_meta_u32(&Database, &str, u32)` and `is_empty(&Database) -> Result<bool>` (the `DOCUMENTS` length is 0). Make the existing `set_meta_u32` / `get_meta_u32` methods delegate to them. The tantivy directory is created **after** this block, so a wipe also clears it.
5. `reindex_fts(doc_id, text)`: `delete_term`, then add one document `doc!(doc_id => doc_id, text => text.to_owned())`.
6. `upsert_document`: drop the vector if the previous record's `text` differs or the record is new. Write `DocRecord { path, text: body }`. Reindex.
7. `remove_document`: remove the record and `vkey(id)`, then `delete_term`.
8. `embed_pending`: pending means a record whose `text.trim()` is non-empty and that has no `vkey(doc_id)`. Embed in batches of 100 as before, keyed by `vkey(doc_id)`.
9. `vec_info`: `pending_count` = (records with non-empty trimmed text) − (vectors present), saturating.
10. `search_fts`:
    - Run the query as now, over-fetching.
    - For each hit, read the record, apply `path_prefix`, and make the snippet:
```rust
    let generator = SnippetGenerator::create(&searcher, &*query, self.fields.text).map_err(tantivy_err)?;
    // per hit:
    let fragment = generator.snippet(&rec.text).fragment().to_owned();
    let snippet = if fragment.trim().is_empty() { leading(&rec.text) } else { collapse_and_cut(&fragment) };
```
    - Results are already one per file, with BM25 descending. Dedupe by `doc_id` defensively, keeping the first, then truncate to `q.limit`.
11. `search_similar`:
    - Scan `VECTORS` with `vkey_parse`, and skip keys that do not parse.
    - Sort by L2 ascending and over-fetch.
    - Resolve the records, apply `path_prefix`, and set `snippet: leading(&rec.text)`.
    - Truncate to `q.limit`.

- [ ] **Step 6: Implement `db.rs`**

- Imports: drop `ChunkHit`. Add `crate::snippet::leading`.
- In `InMemoryStore::search_fts`, set `snippet: leading(&doc.body)` and delete the `chunks` vec.
- In `merge_rrf_files`, delete the `merge_chunk_hits` call and the function itself. The `and_modify` arm only adds to the score. FTS entries are inserted first, so a file found by both keeps its FTS snippet. Add a doc comment line saying so.

- [ ] **Step 7: Implement `lib.rs`**

Remove `pub mod chunker;` and the `chunker::…` re-export. Re-export `retrieve_store::{Document, FileSearchResult, FtsQuery, HybridQuery, RetrieveStore, VectorQuery}`, plus `vector_store::VecInfo` and `snippet::SNIPPET_CHARS`. Delete `chunker.rs`.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p sapphire-framework-retrieve --no-default-features --features redb-store`
Expected: all pass, including the 9 new tests. Then run `cargo check -p sapphire-framework-retrieve --all-features`, which compiles the fastembed path without running it.

- [ ] **Step 9: Commit**

```bash
git add -A crates/sapphire-framework-retrieve
git commit -m "feat(retrieve)!: one document per file — drop the chunker, results carry a snippet" -m "The store records schema_version 2 and rebuilds a cache of the old shape." -m "Refs #184." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: workspace indexing, protocol version, and compile fallout

**Files:**
- Modify: `crates/sapphire-framework-workspace/src/indexer.rs`, `workspace_state.rs`, `lib.rs`
- Modify: `crates/sapphire-framework-backend/src/protocol.rs`
- Modify: whatever else `cargo check --workspace --all-targets` reports (expected: places that read `.chunks` of a `FileSearchResult`, or that build `Document { chunks, .. }`)

**Interfaces:**
- Consumes: the Task 2 types.
- Produces: `build_document_from_disk(path, doc_id) -> io::Result<Document>`, which returns the whole-file document.

- [ ] **Step 1: Write the failing test** (`indexer.rs` tests; create the module if absent)

```rust
    #[test]
    fn every_file_becomes_one_whole_document() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [("a.md", "p1\n\np2"), ("b.jsonl", "{\"x\":1}\n{\"x\":2}"), ("c.toml", "k = 1\n")] {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            let d = build_document_from_disk(&p, 7).unwrap();
            assert_eq!(d.body, body, "{name} is indexed whole and verbatim");
            assert_eq!(d.id, 7);
        }
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sapphire-framework-workspace --no-default-features --features redb-store every_file`
Expected: compile errors from the Task 2 type changes.

- [ ] **Step 3: Implement**

- `indexer.rs`:
  - `build_document_from_disk` reads the file and returns `Document { id: doc_id, body: raw, path: path_str }`.
  - Remove the `JSONL_EXTENSIONS` / `TOML_EXTENSIONS` branching, but only where it served chunking. If the same constants also decide which files are indexable, keep them for that purpose.
  - Update the "Supported file types" doc table: every listed extension is indexed whole.
- `workspace_state.rs`:
  - `on_file_updated` builds the same whole-file `Document`, by calling `crate::indexer::build_document_from_disk` if visibility allows, so there is a single code path.
  - Rewrite its doc comment: a file is re-indexed and re-embedded whole.
  - Remove the chunker imports.
- `lib.rs`: drop `Chunk` and `ChunkHit` from the `sapphire_retrieve` re-export list.
- `backend/src/protocol.rs`:
  - Set `API_VERSION` to `3`, and update its test assertion to `3`.
  - Add a one-line doc note: "3: search results carry `snippet` in place of `chunks` (#184)".
- Fix every remaining compile error the same way: use `snippet` where code read `chunks`, and drop the `chunks` field where code built a `Document`.

- [ ] **Step 4: Verify**

Run:
- `cargo check --workspace --all-targets`
- `cargo test -p sapphire-framework-workspace --no-default-features --features redb-store`
- `cargo test -p sapphire-framework-backend`
- `cargo test -p sapphire-framework-server`

Expected: all compile and pass. The known flake `converge::a_host_that_was_offline_catches_up_when_it_returns` may fail; rerun it alone before you judge it.

- [ ] **Step 5: Commit**

```bash
git add -A crates
git commit -m "feat(workspace)!: index every file whole; protocol 3 for snippet results" -m "Refs #184." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: docs, changelog, final checks

**Files:**
- Modify: `CHANGELOG.md`, `docs/ARCHITECTURE.md`

- [ ] **Step 1: CHANGELOG**

Under the unreleased section's **Breaking** heading, which the file keeps as one section (see commit `5262da9`), add these entries in the file's style:

- The chunker was removed. A file is one document with one vector, and longer input is truncated.
- `FileSearchResult.chunks` and `ChunkHit` are replaced by `FileSearchResult.snippet`. `Document.chunks` and the `Chunker` types are gone.
- The retrieve cache rebuilds itself once on first open (`schema_version` 2).
- The backend protocol is now 3: a CLI and a server must be the same build.
- Downstream note: sapphire-timer's `search` command and sapphire-journal's MCP search output must switch to `snippet`.

- [ ] **Step 2: ARCHITECTURE.md**

The "キャッシュバックエンド" section describes `documents: doc_id -> {path, chunks}` and `vectors: (doc_id,line_start) -> f32[]`. Change both to the per-file layout (`{path, text}`, `doc_id -> f32[]`). Mention `schema_version` and the snippet. Keep the text in Japanese. Remove "チャンク" wording that no longer applies.

- [ ] **Step 3: Final checks**

Run:
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p sapphire-framework-retrieve --no-default-features --features redb-store`
- `cargo test -p sapphire-framework-workspace --no-default-features --features redb-store`

Expected: all clean and passing.

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md docs/ARCHITECTURE.md
git commit -m "docs: one document per file in retrieve; breaking notes" -m "Closes #184." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
