# Changelog

All notable changes to `sapphire-workspace` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.5.0]: https://github.com/fluo10/sapphire-journal/compare/workspace-v0.4.0...workspace-v0.5.0
[0.4.0]: https://github.com/fluo10/sapphire-journal/compare/workspace-v0.3.0...workspace-v0.4.0
[0.3.0]: https://github.com/fluo10/sapphire-journal/compare/workspace-v0.2.0...workspace-v0.3.0
[0.2.0]: https://github.com/fluo10/sapphire-journal/releases/tag/workspace-v0.2.0
