# Synced vectors — implementation plan (#187)

**Spec:** `docs/superpowers/specs/2026-10-10-synced-vectors-design.md`
**Branch:** `feat/issue-187-synced-vectors` (on `feat/issue-186-embedding-settings`)

Each task ends with fmt, clippy (`-D warnings`) and the touched crates' tests green; the last
runs the whole suite.

## Task 1 — the model's full identity

- bridge-api: `EmbedModelInfo` gains `revision: Option<String>` and `max_tokens: Option<u32>`
  (`#[serde(default)]`, skipped when `None`). Every literal updated.
- bridge-embed: `ModelInfo` carries `max_tokens` (`Some` for local, `None` for remote);
  the binary's `ServiceProvider::info` passes it on.

## Task 2 — no conflict copies under `.<app>/embedded/`

- sync: `SyncFilter::allows` refuses a path under `.<app>/embedded/` whose file name
  carries the `.conflict-` tag `merge::conflict_path` writes. Test both sides.

## Task 3 — retrieve: a vector source for `embed_pending`

- `VectorSource` trait (`find`, `may_embed`, `embedded`) and `NoVectorSource`.
- `RetrieveStore::embed_pending(embedder, source, on_progress)`; the redb store asks
  `find` before batching, skips what `may_embed` refuses, calls `embedded` after storing.
  `RetrieveDb::embed_pending` and every caller follow.
- Tests: found vectors are stored without embedding; refused ones stay pending; `embedded`
  sees each computed vector once.

## Task 4 — workspace: vector files

- `vectors.rs`: `Profile`, `VectorDir`, the format (JSON header line + f16 LE), atomic
  writes, `hashes()`. `half` added.
- `WorkspaceState::embed_pending_with(policy)` and `remove_stale_vectors(live, older_than)`;
  `sync_and_embed` / `embed_pending` use the embed-everything policy and keep vector files.
- Tests per the spec.

## Task 5 — server: the embedding pass

- `SyncRuntime::request_embed(root)`: coalescing, one pass at a time per workspace, run in
  the background after scans (handlers and watcher) and after every re-index.
- The pass: `refresh_embedder`; policy from the replica's winners (author is this replica,
  or this host is primary); `embed_pending_with`; on the primary, stale removal.
- Two-host test: the author embeds, the other receives the vector without embedding.

## Task 6 — docs and the whole suite

- ARCHITECTURE: vectors are synced (the index is not); CHANGELOG.
- `cargo test --workspace --no-fail-fast`.
