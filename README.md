# sapphire-workspace

Workspace management library for indexing, search, and sync of Markdown documents.

`sapphire-workspace` ties together [`sapphire-retrieve`] (full-text + vector search) and
[`sapphire-sync`] (git-based synchronisation) into a single, ergonomic API.

## Features

| Feature flag | What it enables | Default |
|---|---|---|
| `lancedb-store` | LanceDB vector backend for semantic search | yes |
| `fastembed-embed` | On-device embedding via FastEmbed | yes |
| `sqlite-store` | SQLite FTS5 + sqlite-vec backend | yes |
| `git-sync` | Git-based workspace synchronisation via `sapphire-sync` | yes |

## Quick start

```toml
[dependencies]
sapphire-workspace = "0.7"
```

### Initialise a workspace

```rust
use sapphire_workspace::{AppContext, Workspace};

let ctx = AppContext::new("my-app");

// Walk up from cwd until a `.my-app` marker directory is found.
let ws = Workspace::find_with_ctx(ctx, std::env::current_dir()?)?;
println!("root: {}", ws.root.display());
println!("uuid: {}", ws.uuid);
println!("cache: {}", ws.ctx.cache_dir().display());
```

### Open and index

```rust
use sapphire_workspace::{AppContext, Workspace, WorkspaceState};

let ctx = AppContext::new("my-app");
let ws = Workspace::find_with_ctx(ctx.clone(), std::env::current_dir()?)?;
let state = WorkspaceState::open(ws, ctx)?;

// Incrementally sync all Markdown / JSON / JSONL files into the retrieve DB.
let (upserted, removed) = state.sync()?;
println!("{upserted} upserted, {removed} removed");
```

### Full-text search

```rust
let results = state.retrieve_db().search("my query", 10)?;
for r in results {
    println!("{}: {}", r.path, r.score);
}
```

### Read files

```rust
use std::path::Path;

// Read a whole file
let content = state.read_file(Path::new("notes/hello.md"))?;

// Read lines 10–20 (1-indexed, inclusive)
let excerpt = state.read_file_range(Path::new("notes/hello.md"), 10, Some(20))?;

// List a directory
for (path, is_dir) in state.list_dir(Path::new("notes"))? {
    println!("{} {}", if is_dir { "d" } else { "-" }, path.display());
}
```

### Write / delete files (index updated automatically)

```rust
use std::path::Path;

state.write_file(Path::new("notes/hello.md"), "# Hello\n\nworld")?;
state.delete_file(Path::new("notes/old.md"))?;
```

## Workspace discovery

A workspace root is detected by walking up the directory tree until a
marker directory is found.  Pass an `AppContext` to every construction
method to set the `app_name` so that marker directories and XDG caches
use the host application's namespace:

```rust
use sapphire_workspace::AppContext;

let ctx = AppContext::new("sapphire-journal");
// marker: {root}/.sapphire-journal/
// cache:  $XDG_CACHE_HOME/sapphire-journal/{uuid}/
```

## Stable workspace UUID

Each workspace directory has a stable [UUIDv8] identifier derived from the
MD5 hash of its canonicalised path.  The UUID is never stored on disk; it is
recomputed on every call to `Workspace::uuid()` or the standalone `path_uuid()`
function.

```rust
println!("{}", sapphire_workspace::path_uuid(Path::new("/my/workspace")));
```

## Configuration

Place `config.toml` inside the marker directory
(`.sapphire-workspace/config.toml`):

```toml
[sync]
backend  = "git"   # "auto" | "git" | "none"
remote   = "origin"
branch   = "main"

[retrieve]
db = "lancedb"   # "none" | "sqlite_vec" | "lancedb"

[retrieve.embedding]
enabled     = true
provider    = "openai"
model       = "text-embedding-3-small"
api_key_env = "OPENAI_API_KEY"
dimension   = 1536
```

Environment variable overrides:

| Variable | Values |
|---|---|
| `SAPPHIRE_WORKSPACE_RETRIEVE_DB` | `none` / `sqlite_vec` / `lancedb` |
| `SAPPHIRE_WORKSPACE_EMBEDDING_ENABLED` | `1` / `true` / `yes` |
| `SAPPHIRE_WORKSPACE_EMBEDDING_PROVIDER` | string |
| `SAPPHIRE_WORKSPACE_EMBEDDING_MODEL` | string |
| `SAPPHIRE_WORKSPACE_EMBEDDING_API_KEY_ENV` | env-var name |
| `SAPPHIRE_WORKSPACE_EMBEDDING_BASE_URL` | URL |
| `SAPPHIRE_WORKSPACE_EMBEDDING_DIMENSION` | integer |

## Supported file types

The indexer walks the workspace root (hidden directories are skipped) and
processes:

| Extension | Chunking strategy |
|---|---|
| `md`, `markdown`, `txt`, `rst`, `org` | Paragraph split; backends auto-chunk |
| `json` | Message/element extraction; each element is a separate chunk |
| `jsonl` | One chunk per line |

## License

Licensed under either of [MIT](../LICENSE-MIT) or [Apache-2.0](../LICENSE-APACHE) at your option.

[`sapphire-retrieve`]: ../sapphire-retrieve
[`sapphire-sync`]: ../sapphire-sync
[UUIDv8]: https://www.rfc-editor.org/rfc/rfc9562#name-uuid-version-8
