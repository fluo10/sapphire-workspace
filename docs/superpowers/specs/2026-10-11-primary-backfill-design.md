# Primary backfill: the primary device fills in missing vectors, gently

Issue: #188. Builds on #182 (primary device) and #187 (synced vectors), which already made
the primary device embed what nobody else does and remove stale vectors.

## What #187 left

- The primary device embeds only when something happens (a scan, a session, becoming
  primary). A file whose author never embedded it waits for the next change.
- The primary device embeds another device's file at once, racing the author, who is
  usually about to embed it. The race is harmless (#187 decision 1) but wastes a model run.
- A backfill of thousands of files goes to the bridge in batches of 100. The bridge has one
  worker, so a search query arriving behind a batch waits for all 100 — minutes with the
  local model.
- Nothing tells the user how far embedding has got.

## Decisions

1. **A periodic pass.** Every 10 minutes the dial loop requests an embedding pass for each
   workspace this host is the primary device of. Event-driven passes stay as they are.
2. **A grace period for other devices' files.** The primary device embeds a file whose
   winning version another device wrote only once that version is 10 minutes old, by its
   HLC wall time (synced, the same on every device). Its author has embedded it by then, or
   will not. The author's own files are embedded at once, as before.
3. **Small batches.** Server passes send 4 texts per request instead of 100, so a query
   waits behind at most 4 documents (about 40 s with the local model on an AVX2 CPU, far
   less with a remote one). An app calling `embed_pending` itself keeps 100.
4. **Progress in `sync.status`.** `SyncStatusResult` gains
   `embedding: Option<EmbeddingProgress { vectors, pending, running }>` — `None` when this
   host does not embed. `workspace list` prints the pending count when there is one, and
   the GUI's workspace row shows it.
5. **A model change re-embeds through the same path.** `configure_vectors` drops the old
   model's vectors, so every file is pending: each device re-embeds the files whose winning
   version it wrote, and the primary device the rest (old versions are past the grace
   period at once), all in small batches, reporting progress. Nothing else is needed.
6. **Unused models' vectors are not removed here.** The issue proposed deleting every
   `embedded/<profile>/` but the current one; that was decided against (keep them, remove
   them only by an explicit command, #206), so trying a model and reverting costs nothing.
   Stale vectors of the current profile are removed as #187 does.
7. **Idempotent, as #182 requires.** Two primaries during a partition embed the same files
   into the same paths (last writer wins, no conflict copies) and remove only vectors the
   backfill can recreate. The loss is computation, never data.

The 10-minute interval and grace are fields of `SyncRuntime` with a test-only setter.

## Changes

- retrieve: `VectorSource::batch_size()` (default 100); the redb store batches by it.
- workspace: `embed_pending_with(policy, batch_size)`; `EmbeddingProgress` from `db_info`.
- backend protocol: `EmbeddingProgress`, `SyncStatusResult.embedding` (`#[serde(default)]`).
- server: the grace rule in the policy, the periodic request in the dial loop, the
  `running` flag, `status` filling `embedding`; CLI and GUI show pending.

## Testing

- retrieve: a source's batch size is the request size.
- server, two hosts: with a long grace the primary does not embed another device's fresh
  file; with none it does; `sync.status` reports pending and vectors.
