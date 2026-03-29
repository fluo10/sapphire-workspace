# sapphire-retrieve

Full-text and semantic search library extracted from [sapphire-journal](https://github.com/fluo10/sapphire-journal).

## What this crate provides

- **FTS5** — trigram full-text search over a SQLite database (`RetrieveDb::search_fts`)
- **Vector search** — approximate nearest-neighbour search via sqlite-vec or LanceDB (`RetrieveDb::search_similar`)
- **Chunker** — splits documents into overlapping text chunks for embedding (`chunker::chunk_document`)
- **Embedder trait** — pluggable embedding backends (`build_embedder`, `EmbeddingConfig`)
  - `openai` — OpenAI-compatible REST API
  - `ollama` — local Ollama server
  - `fastembed` *(feature: `fastembed-embed`)* — local ONNX inference, no server required
- **LanceDB store** *(feature: `lancedb-store`)* — high-performance columnar vector store

## Features

| Feature | Default | Description |
|---|---|---|
| `lancedb-store` | yes | Enable LanceDB as a vector backend |
| `fastembed-embed` | yes | Enable local ONNX embedding via fastembed |

## License

MIT OR Apache-2.0
