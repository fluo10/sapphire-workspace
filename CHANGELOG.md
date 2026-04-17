# Changelog

All notable changes to `sapphire-workspace` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed (breaking)

- `sapphire-retrieve`: full-text search now indexes **chunks** (via `chunks_fts`) instead of whole documents. `search_fts`, `search_similar`, and `search_hybrid` all return `Vec<FileSearchResult>` — each file carries a `chunks` array with the matched line ranges (`line_start`, `line_end`), so MCP / AI callers can see *where* inside a file a match occurred without re-reading the whole document.
- `sapphire-retrieve`: `SearchResult` / `ChunkSearchResult` / `dedup_chunk_results` / `merge_rrf` removed; replaced by `FileSearchResult` / `ChunkHit` / `merge_rrf_files`.
- `sapphire-retrieve`: Query structs introduced (`FtsQuery`, `VectorQuery`, `HybridQuery`) with builder methods. All three take `query: &str` as a common field; `VectorQuery` / `HybridQuery` also accept an `Embedder` (mandatory for vector, optional for hybrid — `None` falls back to FTS-only). Callers no longer pre-compute embeddings.
- `sapphire-retrieve`: `RetrieveStore::search_hybrid` added to the trait with a default implementation, and exposed on `RetrieveDb`.
- `sapphire-retrieve`: `search_fts` / `search_similar` / `search_hybrid` accept an optional `path_prefix` that is pushed down to the backend (SQLite `GLOB`, LanceDB `only_if`), replacing the post-filter that previously lived in `WorkspaceState`.
- `sapphire-retrieve`: chunk schema changed from `(line, column)` to `(line_start, line_end)` — inclusive line range. `TextChunk`, `Chunk`, and all backend schemas updated.
- `sapphire-retrieve`: `documents.body` column / field dropped from storage (still used as chunker input when `Document::chunks` is `None`). SQLite `documents_fts` virtual table removed; LanceDB FTS now indexes `chunks_meta.text`.
- `sapphire-retrieve`: **schema migration required.** SQLite databases on version `<4` are automatically wiped and recreated on first open (next sync re-indexes the workspace). LanceDB is bumped to `lancedb_v4/`; the old `lancedb_v3/` directory is no longer used and can be removed manually.

## [0.8.1] - 2026-04-13

### Fixed

- `sapphire-sync`: SSH credentials callback no longer loops infinitely when `ssh-agent` returns a credential that fails authentication. The callback now tracks attempt index and cycles through methods (ssh-agent → key files) instead of retrying the same method. (#38)
- `sapphire-sync`: remote push rejections are now logged via `tracing::warn` (previously silently ignored due to missing `push_update_reference` callback).

### Added

- `sapphire-sync`: `tracing` instrumentation in `sync_git` for observability (fetch/merge/push cycle start, early-return, push result).

## [0.8.0] - 2026-04-12

### Changed

- `sapphire-sync`: `SyncConfig` split into three types — `WorkspaceSyncConfig` (workspace-level: `backend`, `remote`, `branch`, `sync_interval_minutes`), `UserSyncConfig` (device-level: `device_id`), and `SyncConfig` (flattened combination; TOML `[sync]` section layout unchanged).
- `sapphire-retrieve`: `RetrieveConfig` gains `sync_interval_minutes: Option<u32>` and a `sync_interval()` helper, enabling independent scheduling of the retrieve cache refresh from git sync.
- `sapphire-workspace`: `WorkspaceState::open_configured` now takes `&SyncConfig` instead of `&WorkspaceConfig`.  `load_retrieve_backend`, `load_embedder`, `sync_and_embed`, and `embed_pending` now take `&RetrieveConfig` directly.
- `sapphire-workspace`: added `sync_git(&SyncConfig)` and `sync_retrieve(&RetrieveConfig)` as independent public methods; `periodic_sync()` is now a convenience wrapper over the two.
- `sapphire-workspace`: `src/config.rs` is now re-exports only (`sapphire_retrieve::config` and `sapphire_sync::config`); all config struct definitions live in their home crates.
- `sapphire-workspace-cli`: `UserConfig` (with `load`, `save`, and env-var overrides) moved into the CLI crate.  Layered config loading (`load_layered`) removed — a single user config file is used.
- `sapphire-workspace-cli`: `watch` command now runs two independent timers: one for git sync (`config.sync.sync_interval()`) and one for retrieve cache refresh (`config.retrieve.sync_interval()`).

### Removed

- `WorkspaceConfig` and `UserConfig` removed from the public API of `sapphire-workspace`.
- `.sapphire-workspace/config.toml` is no longer read for sync or retrieve settings (the marker directory is still used for workspace root discovery).

## [0.7.1] - 2026-04-12

### Fixed

- `sapphire-sync`: SSH push and fetch now authenticate correctly via libgit2.  Previously `remote.push()` / `remote.fetch()` were called without `RemoteCallbacks`, causing push to silently fail against SSH remotes.  Authentication is now attempted in order: ssh-agent → `~/.ssh/id_ed25519` → `~/.ssh/id_ecdsa` → `~/.ssh/id_rsa`. (#30)

## [0.7.0] - 2026-04-11

### Changed

- `WorkspaceState::retrieve_db()` now returns `Arc<dyn RetrieveStore>` instead of the concrete `RetrieveDb` type.  Callers that previously called methods on `RetrieveDb` directly should switch to the `RetrieveStore` trait interface.
- `sapphire-retrieve`: added backend factory functions `open_sqlite_fts`, `open_sqlite_vec`, `open_lancedb`, `open_in_memory` — each returns `Arc<dyn RetrieveStore + Send + Sync>` (feature-gated as before).
- `sapphire-retrieve`: `RetrieveDb::dedup_chunk_results` moved to a crate-level free function `dedup_chunk_results`; the method on `RetrieveDb` is kept as a deprecated shim.
- `sapphire-retrieve`: `wipe_db_files` is now `pub` (was `pub(crate)`).
- `sqlite-store` feature is now enabled by default (previously opt-in); the default feature set now includes `sqlite-store`, `lancedb-store`, `fastembed-embed`, and `git-sync`.

### Deprecated

- `RetrieveDb` — use `Arc<dyn RetrieveStore>` returned by `WorkspaceState::retrieve_db()` instead.  `RetrieveDb` re-export is kept for one release to ease migration.

## [0.6.0] - 2026-04-11

### Added

- `WorkspaceState::retrieve_files` — unified search method supporting full-text, semantic, and hybrid (FTS + semantic via Reciprocal Rank Fusion) modes with configurable weights; accepts an optional folder path filter for scoping results.
- `WorkspaceState::sync_workspace_incremental` — mtime-based incremental indexer that only re-indexes files changed since the last sync, making periodic background refreshes much cheaper than a full rescan.
- `WorkspaceState::periodic_sync` — orchestrates a full sync cycle: git sync (if configured) followed by an incremental cache refresh.
- `SyncConfig::device_id: Option<Uuid>` and `SyncConfig::ensure_device_id()` — per-device UUID embedded in git commit messages for tracing sync origin across devices.
- CLI: layered config loading via the `config` crate — workspace-level `{marker}/config.toml` (shared across devices) merged with a per-user override file (`$XDG_CONFIG_HOME/sapphire-workspace/config.toml`).
- CLI: per-device UUID managed in the user-level config; git commits carry the message `auto: sync [<uuid>]`.

### Changed

- `sync_interval_minutes` moved from `SyncConfig` (`sapphire-sync`) to `WorkspaceConfig` (`sapphire-workspace`); periodic sync is now orchestrated by `WorkspaceState` to cover both git sync and cache refresh.

### Removed

- `sapphire_workspace::util::merge_toml_values` — use the `config` crate directly for layered config merging.

## [0.5.1] - 2026-04-08

Internal repository restructure; no public API changes.

## [0.5.0] - 2026-04-08

### Added

- `AppContext` struct — cross-platform cache directory helper; carries `app_name` and computes `cache_dir()` / `model_cache_dir()` on all platforms (XDG on Linux, Platform-specific on macOS/Windows).
- `Workspace::from_root_with_uuid` / `Workspace::find_from_with_uuid` — open or discover a workspace when the UUID is already known (avoids recomputing from the path).
- `Workspace.uuid` stored as a field on construction (previously recomputed on every call to `uuid()`).
- `SyncConfig.sync_interval_minutes: Option<u32>` (in `sapphire-sync`) — configures automatic periodic sync; `sync_interval()` helper returns the value as `std::time::Duration`.

### Changed

- `Workspace::open_with_ctx` / `Workspace::find_with_ctx` and related methods now accept an `AppContext` as the first argument (was a separate `app_name: &'static str` parameter).
- `WorkspaceState` construction methods require an explicit `AppContext`; there is no longer a default (implicit) context.
- `Workspace.ctx` is now a public field so downstream crates can read `app_name` and `cache_dir` directly.
- `SyncConfig` and `SyncBackendKind` moved to `sapphire_sync::config` (public module); `sapphire-workspace` re-exports them via `pub use`.
- `RetrieveConfig`, `VectorDb`, and `EmbeddingConfig` moved to `sapphire_retrieve::config` (public module); `sapphire-workspace` re-exports them via `pub use`.
- `VectorDb` is now a top-level field of `RetrieveConfig` (`retrieve.db` in TOML) instead of nested inside `EmbeddingConfig`.
- `AppContext.cache_dir()` renamed from `cache_base()`; `app_name` is now folded into the cache path automatically.
- `EmbedderConfig.cache_dir: Option<PathBuf>` added to `sapphire-retrieve`; callers inject the model cache directory via `AppContext.model_cache_dir()` instead of relying on `dirs`.

### Removed

- `AppContext::set_model_cache_dir()` — replaced by `set_cache_dir()` which covers the same use-case.
- Implicit default `AppContext` on `WorkspaceState`; callers must supply one explicitly.

## [0.4.0] - 2026-04-06

### Added

- `WorkspaceState::read_file(relative)` — read a workspace-relative text file and return its contents as a `String`.
- `WorkspaceState::read_file_range(relative, start_line, end_line)` — read a line range from a workspace-relative text file (1-indexed, inclusive; `end_line: None` reads to EOF; out-of-bounds lines are silently clamped).
- `WorkspaceState::list_dir(relative)` — list the direct children of a workspace-relative directory, returning `(workspace-relative path, is_dir)` pairs sorted alphabetically.

## [0.3.0] - 2026-04-06

### Added

- `Workspace::find_with_app_name` / `Workspace::find_from_with_app_name` — discover a workspace using a custom app name so that host applications (e.g. `sapphire-journal`) can keep their marker directories and XDG caches in their own namespace.
- `Workspace::from_root_with_app_name` — open a workspace at a known path with a custom app name.
- `path_uuid` free function — compute the stable UUIDv8 workspace identifier without constructing a `Workspace`.  Useful for host crates that share the same cache namespace.

### Changed

- Workspace UUID algorithm switched from UUIDv3 (SHA-1 + external namespace) to **UUIDv8** derived from the MD5 hash of the canonicalised path.  The UUID is now self-contained and does not depend on any compile-time namespace constant.  Existing cached data (SQLite DB, LanceDB directory) must be regenerated after upgrading.
- `Workspace::app_name` is now a `&'static str` instead of a `String`; construction via the `_with_app_name` variants requires a `'static` string literal.
- `Workspace::cache_dir()` now incorporates `app_name` in the XDG path: `$XDG_CACHE_HOME/{app_name}/{uuid}/`.
- Bumped `sapphire-retrieve` and `sapphire-sync` dependencies to `0.3.0`.

### Removed

- Internal `marker` field on `Workspace` replaced by `app_name`; marker directory name is always computed as `".{app_name}"` on the fly.

## [0.2.0] - 2026-04-06

### Added

- Initial public release of `sapphire-workspace`.
- `Workspace` struct with marker-based discovery (`find`, `find_from`, `from_root`).
- `WorkspaceState` — lazily initialises the retrieve DB, embedder, and sync backend.
- `WorkspaceConfig` stored in `{marker}/config.toml` (TOML).
- `UserConfig` legacy fallback loaded from `$XDG_CONFIG_HOME/sapphire-workspace-cli/config.toml`.
- Sync backend selection: `auto` (default), `git`, `none`.
- File-level index helpers: `write_file`, `append_file`, `delete_file`, `on_file_updated`, `on_file_deleted`.
- Bulk indexer (`sync_workspace`) supporting Markdown, plain text, JSON, and JSONL files.
- JSON/JSONL chunking via `sapphire-retrieve`'s `JsonChunker`; source line positions preserved in `ChunkSearchResult`.
- `fastembed-embed`, `lancedb-store`, `sqlite-store`, `git-sync` feature flags.
- Re-exports of `sapphire-retrieve` and `sapphire-sync` public APIs.

[0.8.1]: https://github.com/fluo10/sapphire-workspace/compare/workspace-v0.8.0...workspace-v0.8.1
[0.8.0]: https://github.com/fluo10/sapphire-workspace/compare/workspace-v0.7.1...workspace-v0.8.0
[0.7.1]: https://github.com/fluo10/sapphire-workspace/compare/workspace-v0.7.0...workspace-v0.7.1
[0.7.0]: https://github.com/fluo10/sapphire-workspace/compare/workspace-v0.6.0...workspace-v0.7.0
[0.6.0]: https://github.com/fluo10/sapphire-workspace/compare/workspace-v0.5.1...workspace-v0.6.0
[0.5.1]: https://github.com/fluo10/sapphire-workspace/compare/workspace-v0.5.0...workspace-v0.5.1
[0.5.0]: https://github.com/fluo10/sapphire-journal/compare/workspace-v0.4.0...workspace-v0.5.0
[0.4.0]: https://github.com/fluo10/sapphire-journal/compare/workspace-v0.3.0...workspace-v0.4.0
[0.3.0]: https://github.com/fluo10/sapphire-journal/compare/workspace-v0.2.0...workspace-v0.3.0
[0.2.0]: https://github.com/fluo10/sapphire-journal/releases/tag/workspace-v0.2.0
