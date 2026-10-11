# Changelog

All notable changes to `sapphire-workspace` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

### Added

- External devices (#199): clients that reach the workgroup's applications with a key instead of syncing — a recording pendant, a remote ACP client, a webhook. They live in a synced ledger beside the devices, `<workgroup root>/external_devices/<grain-id>.toml`, each listing the applications it may use (one external device may use several; none by default). The token (`sapphire-ed-…`) is shown once by `add` and `rotate`; the ledger keeps only its SHA-256. `retire` keeps the record, `restore` brings it back, `rotate` keeps the id. Managed with `sapphire-bridge external-device …`, every app's `external-device …` (whose `add` allows that app), and the sync panel's External devices screen. `sapphire-framework-bridge-api` 2.3.0 adds the `external_device.*` methods; `sapphire-framework-registry` adds `ExternalDevices`.

- Primary and secondary devices, elected per workspace from bridge `Hello` messages (`sapphire/hello/1`), with a manual priority: `sapphire-bridge device priority`. `sapphire-framework-bridge-api` 2.1.0 adds `bridge.device_priority_set`, `PeersResult.roles` and `PeerInfo.priority`.
- `sapphire-framework-workspace`: a shared `logging` module — the app log, lifted from the bridge's own `logging` onto the foundation every app server builds on. `AppContext::init` now installs the process's `tracing` subscriber (console on stdout, so journald keeps seeing a service-managed server's output), and `AppServer::run` routes the file layer at `<data dir>/logs/app.log`: the framework's and the app's own targets at info, appending across restarts, rotating at 10 MiB with three rotated files kept. An app server that logged nothing before — no subscriber, so `tracing` silently dropped every event — now logs by building its context, with no wiring of its own. `tail`/`follow` read the file back in the manner of `tail -f`.
- `bridge.workgroup_create` and `bridge.device_retire` on the bridge control plane; the framework and bridge CLIs use them when the bridge runs.
- One workspace per app server (#215). The server keeps the workspace it serves, and whether it syncs, in `<config dir>/workspace.toml`, and comes back to both on start; `workspace.select` switches it without a restart (`workspace.init` and `sync.map` switch to what they make), and `workspace.current` reports it. Only the current workspace syncs on this host. Clients never name a workspace: the `workspace.*` and `sync.*` methods drop `ws` (backend `API_VERSION` 4), `IpcBackend::connect` / `from_client` drop the root, and `IpcBackend::check_cwd` refuses a command run from inside another workspace. The CLI's `workspace` verbs are `init`, `select`, `show` and `map`; the sync panel's Workspace screen shows the current workspace and switches it (open a folder, create one, bring one from the workgroup). A host list (`workspaces.toml`) from a development build migrates once: its first synced workspace (else its first reachable one) becomes the current workspace.
- `sapphire-framework-gui`: `client::FrameworkClient`, `views::*` and `SyncPanel` for workgroup, device and workspace management; every refresh is time-boxed to 5 s and every command to 30 s (join 90 s, service install 5 min), configurable via `ClientConfig::{fetch_timeout, command_timeout, install_timeout}`; `fonts::{system_cjk_font, add_system_cjk_fallback, install_system_cjk_fallback}` (a CJK system font on Windows / macOS / Linux, no bundled assets); `pub use egui`.
- `sapphire-framework-bridge-embed`: the bridge's embedding component — a local Qwen3-VL-Embedding-2B at 1024 dimensions (default, via fastembed/candle) or an OpenAI-compatible endpoint behind one worker. The facade re-exports it as `bridge_embed` under the non-default `bridge-embed` feature (not in `native`: fastembed and candle are heavy); the component is a framework crate because mobile apps embed the bridge in-process.
- `sapphire-framework-bridge-api` 2.2.0: the additive `embed.info` and `embed.embed` control-plane methods, and their client calls.
- Embedding settings (#186): the workgroup configures one local and one remote model (`<workgroup root>/embedding.toml`, synced); each device switches each on or off (`<bridge dir>/embedding.toml`; unset means auto — the local model is off on a CPU without AVX2, the remote one is on) and keeps the remote model's API key to itself (`<bridge dir>/embedding.key`, owner-only, never printed). The remote model is used when it is on, the local one otherwise. Set them with `embedding show | local set|clear|device | remote set|clear|device | key set|clear` on `sapphire-bridge` (which writes the files when no bridge runs) and on every app's CLI, or on the sync panel's new Embedding screen. A running bridge applies a change at once, including one synced from another device, and app servers follow on their next sync.
- Synced vectors (#187): each file's embedding is a file in the workspace, `<root>/.<app>/embedded/<profile>/<sha256>.vec` (a JSON header line, then f16 values), keyed by the file's content hash and a profile naming the model and its settings. It is computed once — by the device that wrote the file's winning version, or by the primary device for everything else — and every other device reads it instead of embedding. The sync filter never makes conflict copies under `.<app>/embedded/`. The primary device removes vectors no live file needs after a day; vectors of a model no longer in use stay. The app server runs these passes in the background after its scans and sessions. `RetrieveStore::embed_pending` takes a `VectorSource`; `WorkspaceState` gains `embed_pending_with`, `remove_stale_vectors` and `refresh_embedder_at`; `EmbedModelInfo` gains `revision` and `max_tokens`.
- Primary backfill (#188): the primary device fills in vectors for other devices' files once their version is ten minutes old (their author has embedded them by then, or will not), on its own every ten minutes as well as after scans and sessions. Server passes send four texts per request, so a search query is never stuck behind a long backfill on the bridge's one worker. `sync.status` reports `embedding` progress (vectors, pending, running), shown by `workspace show` and the sync panel. A model change re-embeds through the same path. `VectorSource::batch_size`; `WorkspaceState::embed_pending_with` takes a batch size; `SyncRuntime::set_backfill_timing`.
- Availability (#190): each bridge records, once a minute, how long it has been running (`<bridge dir>/availability.toml`, per UTC day for the last seven days; sleep is not counted) and announces the share as a tier in Hello — 3 at 99 % or more, 2 at 95 %, 1 at 80 %, 0 below, one lower with under a week of history, none with under an hour. Among candidates of equal priority, the higher tier is elected primary or secondary. The tier is never written to the ledger. `device list` (as `a<tier>`, beside `p<priority>`) and the sync panel's device rows show it.
- `sapphire-framework-bridge-api` 2.3.0: `embed.settings`, `embed.model_set`, `embed.device_set`, `embed.key_set` and `embed.key_clear` with their client calls and types; `EmbedInfoResult.note` says why embedding is off; feature `cli` holds the shared `embedding` subcommands.

### Changed

- `sapphire-framework-keys` authenticates against the workgroup's external devices: `protect(AuthConfig, router)` with a `Verifier`, normally `BridgeVerifier::new(app, version)`, which asks the bridge (`external_device.authenticate`) and caches a success for 30 seconds; a refused token is 401, a bridge that cannot be asked is 503. `Authenticated` now carries the external device's `id` and `name`. `KeyStore`, `KeyEntry` and the per-server `keys.toml` of raw tokens are removed: re-create existing keys as external devices.

- A workspace syncs as a star as soon as a primary device is elected, around its primary and secondary devices. One candidate (a device at priority >= 1, the default) is enough: the devices at priority 0 then sync only through it. Set every device's priority to 0 to keep the full mesh.
- Until availability (#190) exists, ties between equal priorities fall to the device id, which is arbitrary. Raise the priority of your always-on machine so that it is the one elected.
- Raising a priority does not move a role that is already held: the election is non-preemptive, so the new priority counts only when the role is next free.
- `workspace show` shows the server's workspace, then the workgroup's workspaces of the app.
- `StatusReport` / `StatusRow` live in `backend::protocol` (still re-exported from the server crate).
- `sapphire-framework-bridge-api` is versioned on its own (2.0.0; its major is the control-plane `API_VERSION`), no longer `version.workspace`.

### Breaking

- Backend `API_VERSION` 3 (search results carry a `snippet` instead of chunks) and bridge-api 2.0.0 (`API_VERSION` 2): replace and restart every CLI, server and bridge on a host together; a CLI and a server must be the same build.
- Backend `API_VERSION` 4 (#215): no `ws` parameter; `workspace.select` and `workspace.current` replace `workspace.list` and `workspace.forget`. `WorkspaceHost` holds one workspace (`DEFAULT_MAX_OPEN`, `DEFAULT_IDLE` and `AppServer::limits` are gone); `HostRegistry` is gone; `sync_router` and `workspace_router` take a `Current`. Upgrade note: a server comes up with no workspace until one is selected — run `<app> workspace select <dir>` (or `init`) once, or pick it in the GUI.
- The chunker was removed from `sapphire-framework-retrieve`: a file is one document with one vector, and longer input is truncated.
- `FileSearchResult.chunks` and `ChunkHit` are replaced by `FileSearchResult.snippet`; `Document.chunks` and the `Chunker` types are gone. Downstream: sapphire-timer's `search` command and sapphire-journal's MCP search output must switch to `snippet`, and sapphire-journal-core no longer compiles until it drops `chunks: None` from the `Document` it builds (`crates/sapphire-journal-core/src/cache.rs`).
- The retrieve cache rebuilds itself once on first open (the retrieve store's own `schema_version`, now 2; unrelated to the workspace crate's `RETRIEVE_SCHEMA_VERSION`): the old store is wiped, and the first sync after it — incremental or full — re-indexes the whole workspace. `WorkspaceState` clears the track store whenever it finds an empty retrieve store next to recorded file stamps, which also covers the redb `UpgradeRequired` reset.
- `sapphire-framework-session`: `run_session` now takes `&tokio::sync::Mutex<Replica>` instead of `&mut Replica`. It locks the replica itself, only from after the peer's `Hello` is in through to the end of the exchange — not while waiting for it — so a caller that used to pre-lock a `Mutex<Replica>` around the whole call should pass the mutex itself instead: pre-locking defeats the fix, pinning the replica against a concurrent scan or another session for as long as a slow-to-speak peer is waited on (issue #163).
- Embedding moved to the bridge: one model per host, and apps ask it over IPC (`embed.info` / `embed.embed`) instead of each building and linking their own embedder. No embedding is a normal state — a missing or disabled configuration, a stopped bridge, or a model that failed to load all mean search falls back to FTS only, not an error.
- `RetrieveConfig.embedding`, `EmbeddingConfig` and `build_embedder` were removed, and so was the `fastembed-embed` feature (`sapphire-framework-retrieve` keeps only the `Embedder` trait; `RetrieveConfig` keeps `db` and `hybrid`). Configure embedding on the bridge (see the embedding settings entry above). `WorkspaceState::load_embedder()` now takes no config argument: it asks the bridge and configures the vector store with the reported model and dimension (`configure_vectors`); apps pass only the vector DB kind via `WorkspaceState::set_vector_db(RetrieveConfig.db)`. `WorkspaceState::embedder()` returns an `Arc` (the embedder can be replaced while a caller holds it), and `sync_and_embed` asks the bridge again every time (`refresh_embedder`), following it to another model or to none.
- sapphire-journal follow-up needed: it configures `[cache.retrieve.embedding]` and calls `build_embedder` with the `fastembed-embed` feature, all of which is gone — a follow-up issue in that repository tracks the migration.

### Fixed

- A device that is neither primary nor secondary no longer loses every sync session during a primary handover (#192). It keeps the sessions the star would close until it holds one with a primary or secondary device of its own view, so a stale view for one Hello round cannot leave it without a path.
- `sapphire-framework-bridge`: a join whose device name differs from the one the invite was issued for is refused before the invite is spent (`Invites::redeem_as`), so the joiner can retry with the right name instead of needing a new ticket.
- `sapphire-framework-bridge`: an inbound stream for the workgroup's own workspace no longer blocks the accept loop — and with it every other peer's stream, for any other workspace — until its replication session finishes or times out. The session now runs on its own task; the replica's own lock still serialises these sessions one at a time, so nothing runs more concurrently than before, but the accept loop itself is free to move on (#164).
- `sapphire-framework-sync`: a scan's "known content → settle, don't record" short-circuit (added for #157) matched a local write against *any* sibling version at the path, not just the current winner. A write that changed which content was at a path, but happened to land on bytes already present as a *losing* conflict sibling, was discarded instead of recorded — destroying the previous winner's only copy (the file) with nothing recording that it was superseded, and no conflict copy to fall back on (conflict copies exist for losers, never for the winner). Now matched against the winner specifically, which is also what the branch's own premise (disk already holds the version the store knows, only the bookkeeping is stale) actually describes (#161).
- `sapphire-framework-bridge`: the data plane's relay now ends as soon as either direction ends, instead of waiting for both. A peer stream the far bridge has no route for — the state two hosts pass through whenever one enables a workspace before the other — left the relay open on the near side for ever, because the app-server-to-peer direction only ends when the app server closes and the app server was the one waiting for a `Hello`. The dialling app server therefore waited on a peer that was already gone, holding its replica's lock while it did: on a transport with no half-close to fall back on (Windows named pipes) for ever, and elsewhere as a bare `expected Hello, got None`. The inbound loop also logs the inbound streams it hangs up on, which is what explains the retry traffic that follows (#159).
- `sapphire-framework-session`: the initial exchange now gives up on a peer that never sends `Hello`, after `HELLO_TIMEOUT`, rather than waiting on it for ever while holding the replica's lock (#159).
- `sapphire-track`: change detection no longer misses edits made within the same second as the last scan. Stored mtimes are now nanosecond-resolution and the stored file size is compared too; the track snapshot value format changed accordingly, so `Workspace::track_db_path()` moved to `track_v2.redb` and pre-existing `track_v1.redb` snapshots are orphaned (one full re-index rebuilds them). Consumers of `TrackStore::mtimes()`/`Observed` must adapt to the new `(mtime_ns, len)` value (#118).
- `sapphire-framework-ipc`: the Windows named-pipe name now includes a hash of the endpoint directory, so same-name endpoints in different directories no longer collide. The pipe-name shape changed, so on Windows an old client and a new server will not find each other (#151).

## [0.12.0](https://github.com/fluo10/sapphire-workspace/compare/v0.11.0...v0.12.0) - 2026-05-23

### Changed

- `sapphire-sync`: bump `git2` from 0.20 to 0.21. This pulls in a new major of `libgit2-sys` (a `-sys` crate with a `links` key), released as a minor bump to avoid silent build failures for downstream crates that depend on a different `libgit2-sys` major. See `RELEASING.md` for the `-sys` policy.

### Internal

- Adopt release-plz for automated version bumps, CHANGELOG generation, and crates.io publishing.
- Add `.DS_Store` to `.gitignore`.

## [0.11.0] - 2026-05-16

### Changed (breaking)

- `sapphire-retrieve`: `JsonChunker` is renamed to `JsonlChunker` and reduced to per-line JSON parsing with a raw-text fallback for partial writes. The previous array / nested-messages support was unused outside the JSONL path. (#53)
- `sapphire-workspace`: plain `.json` files are no longer indexed. The bulk indexer and `on_file_updated` now skip them; only `.jsonl` is processed through the JSONL chunker. (#53)
- `sapphire-workspace`: existing retrieve indexes built on a previous version will have stale chunk boundaries for `.jsonl` files. A re-index is recommended after upgrading so that subsequent appends are picked up incrementally rather than re-embedding the file.

### Performance

- `sapphire-workspace`: `on_file_updated` pre-chunks `.jsonl` files line-by-line instead of falling back to the storage-layer paragraph chunker. Previously every append shifted chunk boundaries and re-embedded most of the file; now existing lines retain their `(doc_id, line_start)` identity and only the appended lines are embedded. (#53)

### Changed

- `sapphire-retrieve`: bump `lancedb` from 0.27 to 0.29 and `arrow-array` / `arrow-schema` from 57 to 58 in lockstep. (#55)
- `sapphire-workspace`: bump `md-5` from 0.10 to 0.11. (#52)

## [0.10.1] - 2026-04-22

### Fixed

- `sapphire-workspace`: `WorkspaceState::write_file` could not create new files — `canonicalize_or_parent` returned paths with a trailing separator (e.g. `/workspace/memory/daily/2026-04-19.md/`) for not-yet-existing files, and `std::fs::write` failed with `EISDIR`. `Path::join` with an empty path appends a separator; the walk-up loop now seeds its suffix with `PathBuf::from(name)` on the first iteration. Existing files were unaffected because the `canonicalize()` fast-path skipped the buggy branch. (#48)

## [0.10.0] - 2026-04-20

### Changed (breaking)

- `sapphire-sync`: `SyncConfig` collapsed back into a single flat struct (`backend`, `remote`, `branch`); `WorkspaceSyncConfig` and `UserSyncConfig` are gone. The workspace/user split no longer carries meaningful distinction once `device_id` moves out of config. (#45)
- `sapphire-sync`: `sync_interval_minutes` dropped from `SyncConfig`; periodic cadence is the host app's concern (the CLI exposes it as `UserConfig::sync_interval_minutes`).
- `sapphire-retrieve`: `RetrieveConfig::sync_interval_minutes` and `sync_interval()` removed.
- `sapphire-workspace`: `WorkspaceState::sync_git` / `sync_retrieve` / `periodic_sync` helpers removed. `watch` in the CLI collapses its two independent timers into a single `periodic_sync()` tick backed by `sync()`.
- `sapphire-workspace`: `WorkspaceState::open_configured` no longer takes a device id / defaults pair — it reads them from the workspace's `AppContext`.
- `sapphire-workspace`: `device_id` is no longer stored in `config.toml`. `AppContext::device_id()` is now a get-or-create accessor backed by `<data_dir>/device_id`; first call generates a UUIDv7 and persists it.
- `sapphire-workspace`: `AppContext` gains `data_dir` / `set_data_dir`, and the library's `dirs` dependency is dropped. Host apps are expected to resolve and inject both `cache_dir` and `data_dir` at startup so the library stays portable to mobile sandboxes.
- `sapphire-sync`: git auto-sync commits now use an RFC 822 / `git interpret-trailers` compatible `Device-Id:` trailer (message becomes `auto: sync by [<uuid>]\n\nDevice-Id: <uuid>`). `GitSync` exposes only `with_device_id(Uuid)`; commit formatting is fully encapsulated.

### Added

- `sapphire-sync`: `DeviceRegistry` backed by a JSONL file (`{marker}/devices.jsonl`) recording each device's hostname, app id/version, platform, arch, and `registered_at` / `updated_at` timestamps. Enables reverse lookup from commit UUIDs and a stable 1-based Device Number derived from UUIDv7 order. (#46)
- `sapphire-workspace`: `DeviceContext` (process-wide device state) on `AppContext` via `set_device_defaults`, `device()`, and `update_device_name_if_newer`. Host-detected fields (hostname, app_id, app_version, platform, arch) are always refreshed from the running binary on open; only user-editable fields (`name`, `updated_at`) follow the "newer `updated_at` wins" rule, so an app-version bump no longer touches `updated_at`.
- `sapphire-workspace-cli`: `device {list, set-name, show}` subcommands exposing the registry; renames are staged via the git backend so the next `sync` propagates them.
- `sapphire-workspace-cli`: top-level `tracing_subscriber` initialised once in `main()`; device-id persistence failures now surface via `tracing::error!` instead of ad-hoc `eprintln!`.

### Renamed

- `sapphire-sync`: `client` / `client_version` renamed to `app_id` / `app_version` across the device registry — this is a local-first app, not client-server, and the new names match `env!("CARGO_PKG_NAME")` / `env!("CARGO_PKG_VERSION")`.

## [0.9.0] - 2026-04-18

### Changed (breaking)

- `sapphire-retrieve`: full-text search now indexes **chunks** (via `chunks_fts`) instead of whole documents. `search_fts`, `search_similar`, and `search_hybrid` all return `Vec<FileSearchResult>` — each file carries a `chunks` array with the matched line ranges (`line_start`, `line_end`), so MCP / AI callers can see *where* inside a file a match occurred without re-reading the whole document.
- `sapphire-retrieve`: `SearchResult` / `ChunkSearchResult` / `dedup_chunk_results` / `merge_rrf` removed; replaced by `FileSearchResult` / `ChunkHit` / `merge_rrf_files`.
- `sapphire-retrieve`: Query structs introduced (`FtsQuery`, `VectorQuery`, `HybridQuery`) with builder methods. All three take `query: &str` as a common field; `VectorQuery` / `HybridQuery` also accept an `Embedder` (mandatory for vector, optional for hybrid — `None` falls back to FTS-only). Callers no longer pre-compute embeddings.
- `sapphire-retrieve`: `RetrieveStore::search_hybrid` added to the trait with a default implementation, and exposed on `RetrieveDb`.
- `sapphire-retrieve`: `search_fts` / `search_similar` / `search_hybrid` accept an optional `path_prefix` that is pushed down to the backend (SQLite `GLOB`, LanceDB `only_if`), replacing the post-filter that previously lived in `WorkspaceState`.
- `sapphire-retrieve`: chunk schema changed from `(line, column)` to `(line_start, line_end)` — inclusive line range. `TextChunk`, `Chunk`, and all backend schemas updated.
- `sapphire-retrieve`: `documents.body` column / field dropped from storage (still used as chunker input when `Document::chunks` is `None`). SQLite `documents_fts` virtual table removed; LanceDB FTS now indexes `chunks_meta.text`.
- `sapphire-retrieve`: **schema migration required.** SQLite databases on version `<4` are automatically wiped and recreated on first open (next sync re-indexes the workspace). LanceDB is bumped to `lancedb_v4/`; the old `lancedb_v3/` directory is no longer used and can be removed manually.
- `sapphire-retrieve`: `title` field removed from `Document` and `FileSearchResult`; `doc_title` dropped from `Chunk` and all backend schemas. Embedding text no longer prepends the title. Display names should be resolved by the application layer from `path`. (#43)

### Added

- `sapphire-workspace`: file operation methods (`read_file`, `read_file_range`, `write_file`, `append_file`, `delete_file`) now validate that paths resolve within the workspace root, preventing path traversal attacks. A new `allow_external_paths` flag on `AppContext` lets applications opt in to external file access — external files use plain `std::fs` without index or sync updates. (#42)

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

[0.11.0]: https://github.com/fluo10/sapphire-workspace/compare/v0.10.1...v0.11.0
[0.10.1]: https://github.com/fluo10/sapphire-workspace/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/fluo10/sapphire-workspace/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/fluo10/sapphire-workspace/compare/workspace-v0.8.1...workspace-v0.9.0
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
