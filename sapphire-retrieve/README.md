# sapphire-retrieve

Full-text and semantic search library extracted from [sapphire-journal](https://github.com/fluo10/sapphire-journal).

## What this crate provides

- **FTS5** — trigram full-text search over a SQLite database (`RetrieveDb::search_fts`)
- **Vector search** — approximate nearest-neighbour search via sqlite-vec or LanceDB (`RetrieveDb::search_similar`)
- **Chunker** — splits documents into overlapping text chunks for embedding (`chunker::chunk_document`)
- **Embedder trait** — pluggable embedding backends (`build_embedder`)
  - `openai` — OpenAI-compatible REST API
  - `ollama` — local Ollama server
  - `fastembed` *(feature: `fastembed-embed`)* — local ONNX inference, no server required
- **Config types** — `RetrieveConfig`, `VectorDb`, `EmbeddingConfig` in `sapphire_retrieve::config`
- **LanceDB store** *(feature: `lancedb-store`)* — high-performance columnar vector store

## Features

| Feature | Default | Description |
|---|---|---|
| `sqlite-store` | no | SQLite FTS5 + sqlite-vec backend |
| `lancedb-store` | yes | LanceDB vector backend |
| `fastembed-embed` | yes | Local ONNX embedding via fastembed |

## License

MIT OR Apache-2.0
