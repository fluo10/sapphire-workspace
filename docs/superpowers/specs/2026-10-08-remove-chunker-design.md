# Remove the chunker: one file, one document, one vector

- Date: 2026-10-08
- Issue: #184 (tracking: #189)
- Scope: `sapphire-framework-retrieve` (chunker removal, store schema, result type, snippets,
  REST input cap), `sapphire-framework-workspace` (indexing), `sapphire-framework-backend`
  (protocol version), docs. Other repositories follow in their own changes.
- Related: #185 (embedding moves to the bridge, token-level truncation), #187 (one vector
  file per file content, synced).

## Background

Retrieval splits each file into chunks with a format-specific chunker: paragraphs for
Markdown, one message per line for JSONL, the whole file for TOML. Each chunk is indexed
and embedded separately, and results report the matched chunks with their line ranges.

Two things make chunking unnecessary and now harmful:

1. **Files are getting small.** Sync works per file. To avoid conflicts, apps are moving to
   many small files, for example agent session logs going from one JSONL file to one TOML or
   Markdown file per message. A file is already about the unit a chunk used to be.
2. **Vectors are about to be synced per file (#187).** A vector file keyed by the file's
   content is only portable if every device derives the same vectors from the same file.
   App-specific chunk boundaries would make that a versioning problem. One vector per file
   removes the problem.

## Decisions

1. **One file = one document = one vector.** Text files are embedded whole. Input longer
   than the model's context is truncated: the tail is dropped.
2. **Results keep a snippet, not chunks.** `FileSearchResult` becomes
   `{ id, path, score, snippet }`. Line numbers go away. With small files, a path plus a
   snippet is enough to show and to act on.
3. **The cache is rebuilt, not migrated.** The store records a schema version. A store of
   the old shape is wiped and re-indexed, exactly as a cache that never existed would be.
4. **Exact token-level truncation is #185's job.** Here, the REST embedder gets a
   conservative character cap so long files do not make the API call fail. fastembed
   already truncates to its model's maximum length.

## Public API (`sapphire-framework-retrieve`)

Removed:

- module `chunker`, and with it `Chunker`, `TextChunk`, `MarkdownChunker`, `JsonlChunker`,
  `TomlChunker` and `chunk_document`;
- `ChunkHit`, and `vector_store::Chunk` together with the chunk-grouping helpers that only
  served chunk results (`ChunkRow` and `group_by_file` are replaced by per-document
  equivalents);
- `Document::chunks`.

Changed:

```rust
pub struct Document {
    pub id: i64,
    /// The text that is indexed and embedded: the file's content.
    pub body: String,
    /// Absolute file path (shown in search results).
    pub path: String,
}

pub struct FileSearchResult {
    pub id: i64,
    pub path: String,
    /// FTS: BM25 score (higher is better). Vector: L2 distance (lower is better).
    /// Hybrid: RRF score (higher is better).
    pub score: f64,
    /// A short excerpt of the file, at most `SNIPPET_CHARS` characters (see below).
    pub snippet: String,
}
```

`VecInfo` keeps its fields. `vector_count` and `pending_count` now count documents.

`merge_rrf_files` merges per file only. There are no chunk lists to deduplicate.

## Store (`RedbStore`)

- redb `documents`: `doc_id -> serde_json { path, text }`. `text` is the full body. The
  store needs it for snippets and for embedding.
- redb `vectors`: key is `doc_id` (8 bytes, i64 LE), value is the f32 blob as before.
- tantivy: one document per file, with fields `doc_id` (indexed + stored) and `text`
  (trigram, not stored). The `line_start` field is removed.
- `meta`: a new `schema_version` key with value `2`. On open:
  - if the key is missing or older, and the store is not empty, the store directory is
    wiped and recreated (the same recovery `create_or_reset` already does for redb's
    `UpgradeRequired`), then `schema_version = 2` is written;
  - a fresh store is simply stamped with `2`.
- `embed_pending` embeds one text per document. A document is pending when it has no
  vector.
- The in-memory store follows the same shape.

## Snippets

`pub const SNIPPET_CHARS: usize = 150;`

- **FTS hit.** Build a tantivy `SnippetGenerator` for the query over the `text` field. Feed
  it the body read from redb; the generator works on any text passed in, so tantivy does
  not need to store the body. Take the fragment, without highlight markup, cut to
  `SNIPPET_CHARS`. If the generator returns an empty fragment, fall back to the leading
  text.
- **Vector hit.** The first `SNIPPET_CHARS` characters of the body.
- **Hybrid.** Use the FTS snippet when the file was an FTS hit, else the leading text.
- Cutting happens on `char` boundaries, never inside a UTF-8 sequence. Runs of whitespace,
  including newlines, are collapsed to single spaces, so a snippet prints on one line.

## Input cap for REST embedders

`const MAX_REST_EMBED_CHARS: usize = 4_000;`. The OpenAI-compatible and Ollama paths
truncate each input to this many characters, on a `char` boundary, before sending. It is a
safety net for the 8,191-token input limit: in cl100k a Japanese kanji is often 2–3
tokens, so 4,000 characters leaves a margin even for dense CJK text. #185 replaces it with
token-level truncation.

A batch is sent in sub-requests of at most 200,000 characters in total
(`MAX_REST_REQUEST_CHARS`), and the results are concatenated in input order, so the
`Embedder` contract (one vector per input, in order) holds.

## Workspace (`sapphire-framework-workspace`)

`indexer::build_document_from_disk` and `WorkspaceState::on_file_updated` stop branching on
extension. Every indexable file becomes `Document { id, body: <file content>, path }`.
JSONL files are therefore indexed and embedded as raw JSON lines. That is acceptable under
decision 1, because apps are moving off large JSONL files. The re-exports in `lib.rs` drop
the removed types. Doc comments that describe chunking are updated.

An append to a file now re-embeds the whole file. That used to be avoided for JSONL by
keeping the line identity stable. Small files make this cheap, and #187 will key vectors by
content anyway.

## Backend protocol

`workspace.search` returns `FileSearchResult` values, whose shape changes. The backend
`API_VERSION` therefore goes from `2` to `3`, so a CLI and a server of different builds
refuse each other at the handshake instead of failing to decode.

## Other repositories (follow-ups, not part of this change)

- **sapphire-timer:** `cli/src/commands/search.rs` prints `file.chunks[..].text`. It must
  print `file.snippet` instead. The doc comment in `core/src/session.rs` mentions
  `JsonlChunker`.
- **sapphire-journal:** the CLI prints path and score only and is unaffected. The MCP
  search tool serializes the results, so its JSON shape changes: `snippet` replaces
  `chunks`.
  - `crates/sapphire-journal-core/src/cache.rs` builds `Document { …, chunks: None }`.
    That no longer compiles: drop the `chunks` field.
  - `cli/src/commands/cache.rs` prints "embedding chunks". This is wording only: it
    should say "embedding documents" (or similar).

Each is a small change in its own repository, made when that repository moves to this
framework revision.

## Error handling

- A store whose schema version cannot be read (corrupt meta) is treated like an old
  version: it is wiped and rebuilt.
- Embedding failures behave as before: the document stays pending. A failed batch is retried one document at a time, so one rejected input does not hold back the rest; `embed_pending` returns the number embedded, and an error only when nothing could be embedded.

## Testing

- Store round trip: upsert, FTS search and vector search return one result per file, with
  a non-empty snippet.
- FTS snippet: contains the query term and is at most `SNIPPET_CHARS` chars.
- Vector snippet: equals the leading text.
- Snippet on multibyte text, where Japanese text is longer than the cap: no panic, valid
  UTF-8, whitespace collapsed.
- Old schema: a store directory with no `schema_version` and some documents is wiped and
  reopened empty, stamped `2`. A fresh store is stamped without wiping.
- REST cap: an input longer than `MAX_REST_EMBED_CHARS` is truncated on a char boundary.
  The helper is unit-tested; no network is involved.
- Hybrid merge per file.
- The existing workspace, server and backend tests pass after adapting them to the new
  types.

## Out of scope

- Token-level truncation and the embedding pipeline: #185.
- Synced vector files: #187.
- Changes in the timer and journal repositories: listed above as follow-ups.
