# sapphire-framework

Local-first framework for file-based workspaces: indexing, search, sync, and (planned)
remote / WASM backends. Formerly published as `sapphire-workspace`; the reusable crates
now live here under the `sapphire-framework-*` prefix.

The `sapphire-framework-workspace` crate ties `sapphire-framework-retrieve`
(full-text + vector search) into a single, ergonomic API over file-based
documents. Concurrent editing is handled by the central remote server
(`sapphire-framework-remote-*`), not by local auto-sync; git is used manually.

## Features

| Feature flag | What it enables | Default |
|---|---|---|
| `redb-store` | redb records + tantivy full-text index + brute-force vectors | yes |
| `fastembed-embed` | On-device embedding via FastEmbed | yes |

## Quick start

```toml
[dependencies]
sapphire-workspace = "0.10"
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
use the host application's namespace.  Initialise the context once at
startup with `init(AppKind::…)`; it resolves the cache / data / config
directories to the per-binary-type layout `<platform-root>/<app-name>/<kind>/`
(first writer wins) and applies the one-shot directory migration:

```rust
use sapphire_workspace::{AppContext, AppKind};

let ctx = AppContext::new("sapphire-journal");
ctx.init(AppKind::Server);
// marker: {root}/.sapphire-journal/
// cache:  $XDG_CACHE_HOME/sapphire-journal/server/{uuid}/
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
[retrieve]
db = "redb"   # "none" | "redb"

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
| `SAPPHIRE_WORKSPACE_RETRIEVE_DB` | `none` / `redb` |
| `SAPPHIRE_WORKSPACE_EMBEDDING_ENABLED` | `1` / `true` / `yes` |
| `SAPPHIRE_WORKSPACE_EMBEDDING_PROVIDER` | string |
| `SAPPHIRE_WORKSPACE_EMBEDDING_MODEL` | string |
| `SAPPHIRE_WORKSPACE_EMBEDDING_API_KEY_ENV` | env-var name |
| `SAPPHIRE_WORKSPACE_EMBEDDING_BASE_URL` | URL |
| `SAPPHIRE_WORKSPACE_EMBEDDING_DIMENSION` | integer |

## Supported file types

The indexer walks the workspace root (hidden directories are skipped) and
processes:

| Extension | Indexing |
|---|---|
| `md`, `markdown`, `txt`, `rst`, `org` | Indexed as one document per file |
| `json` | Message/element extraction; indexed as one document per file |
| `jsonl` | Indexed as one document per file |

## License

Licensed under either of [MIT](../LICENSE-MIT) or [Apache-2.0](../LICENSE-APACHE) at your option.

[`sapphire-retrieve`]: ../sapphire-retrieve
[UUIDv8]: https://www.rfc-editor.org/rfc/rfc9562#name-uuid-version-8
