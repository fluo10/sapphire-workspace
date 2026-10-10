# Synced vectors: one embedding per file, shared by every device of the workgroup

Issue: #187. Builds on #185 (embedding in the bridge) and #186 (embedding settings). Takes in
the smallest part of #188 (the primary device fills in what nobody else embeds) and the
storage side of #206 (vectors kept per model). Spike: #183.

## Why

Today every device embeds every file it indexes. A document costs 8–12 s on an AVX2 CPU
with the local model (#183) and real money with a remote one, so a workgroup of three
devices pays three times for the same vector — and a device without AVX2 cannot pay at all.
A vector depends only on the file's content and the model; it can be computed once and
synced like the file. That revises ARCHITECTURE's rule that the vector index is not synced:
the *index* (redb) stays local, the *vectors* become files in the workspace.

## Decisions

1. **One vector file per content, per model profile:**
   `<root>/.<app>/embedded/<profile>/<sha256>.vec`.
   - `<sha256>` is the hex SHA-256 of the file's bytes — the hash the sync layer already
     addresses content by. A rename needs no new vector, and two files with the same
     content share one.
   - `<profile>` names everything else the vector depends on (decision 2). Together the two
     are the *input hash* #183 asked for: two vectors at the same path were computed from the
     same input, so whichever one last-writer-wins keeps is right, although CPUs differ in
     the last bits.
2. **A profile is the model and its settings:**
   `<slug>-<dimension>-<hash8>`, e.g. `qwen3-vl-embedding-2b-1024-3f9a01c2`. The slug is the
   model name's last segment, lowercased, for people browsing the directory; `hash8` is the
   first 8 hex digits of SHA-256 over the model, its revision, the dimension, the token
   limit and the template version, so that a change to any of them gets a new directory.
   The bridge reports all five in `EmbedModelInfo` (`revision` and `max_tokens` are added,
   optional, in bridge-api 2.3.0).
3. **The file is a JSON header line, then the values as little-endian f16.**
   ```text
   {"format":1,"model":"Qwen/Qwen3-VL-Embedding-2B","revision":null,"dtype":"f16","dim":1024,"max_tokens":1024,"template_version":1,"content":"<sha256>"}\n
   <dim × 2 bytes>
   ```
   About 2.2 KB at 1024 dimensions. Vectors are computed in f32 and rounded only to store
   them; the cosine error this adds is around 1e-3. The reader checks the header against the
   profile and the byte count against `dim`, and treats a file that fails either check as
   absent.
4. **Who embeds a file without a vector.** For a synced workspace, two devices only:
   - **the author** — the device whose replica wrote the version that won (the sync
     state's `Dot.replica` of the path's winner), so an edit is embedded where it was made;
   - **the primary device** of the workspace (#182), for everything else: files written by a
     device whose embedding is off, files that existed before vectors synced, and edits
     whose author went away before embedding them. This is #188 at its smallest; #188 keeps
     the rest (scheduling, progress, re-embedding on a model change).

   Every other device waits for the vector file to arrive, and searches by FTS for that file
   meanwhile. A workspace that is not synced has one device, which embeds everything — the
   behaviour `sync_and_embed` keeps for apps that call it without a server.
5. **A vector that arrives is only read.** Indexing looks for the file's vector before it
   asks for an embedding, and copies what it finds into the local index (redb). Nothing
   re-embeds a file whose vector is present.
6. **Embedding never holds up a save.** The server runs one embedding pass at a time per
   workspace, after its scan or session has re-indexed, on a blocking thread. A pass that
   finds nothing to do costs one directory read per profile.
7. **No conflict copies under `.<app>/embedded/`.** Two devices may write the same vector
   file (the author and the primary, racing). Their contents are equivalent by decision 1, so
   the sync filter refuses the conflict copy there and the newest version wins, as the
   workgroup root already does with its `.sapphireignore`. This one is built into the
   filter, because a user's `.sapphireignore` must not be able to turn it off.
8. **Stale vectors are removed by the primary device only.** After an edit or a delete the
   old content's vector is no longer anyone's. The primary device removes, in the active
   profile only, the vector files whose hash no live file has and which are older than one
   day: a vector can arrive before the file it belongs to, and one day is far longer than
   that window. One device deciding means no two devices race to delete. Vectors of a
   profile that is no longer active are never removed automatically (#206 adds an explicit
   command), so trying a model and returning to the previous one costs nothing.
9. **`.<app>/embedded/` is not indexed.** The indexer already skips hidden directories;
   this is pinned by a test.

## Components

### bridge-api

`EmbedModelInfo` gains `#[serde(default)] revision: Option<String>` and
`#[serde(default)] max_tokens: Option<u32>`. The bridge fills `max_tokens` for the local
model; neither model has a pinned revision yet, so `revision` is `None` and the profile
records that.

### workspace: `vectors` (new module)

- `Profile::of(&EmbedModelInfo) -> Profile` — the directory name and the header fields.
- `VectorDir { root, profile }` with `path(hash)`, `read(hash) -> Option<Vec<f32>>`,
  `write(hash, &[f32])` (atomic: a temporary file outside the synced tree's view, then
  rename), and `hashes()` for the cleanup.
- `encode` / `decode` of the format above, with `half` for f16.

### retrieve

`RetrieveStore::embed_pending` takes a `VectorSource` in addition to the embedder:

```rust
pub trait VectorSource: Sync {
    /// The stored vector for this document's text, if one exists.
    fn find(&self, path: &str, text: &str) -> Option<Vec<f32>>;
    /// Whether this device should compute a vector it could not find.
    fn may_embed(&self, path: &str, text: &str) -> bool;
    /// A vector was just computed: keep it.
    fn embedded(&self, path: &str, text: &str, vector: &[f32]);
}
```

The store keeps its batching and its #194 failure rules; it only asks the source first and
tells it after. `NoVectorSource` (find nothing, embed everything, keep nothing) is today's
behaviour.

### workspace: `WorkspaceState`

- `embed_pending_with(policy)` builds a `VectorSource` over the active profile's
  `VectorDir`, whose `may_embed` is the caller's policy: a closure from the file's
  workspace-relative path and content hash to a yes or no.
- `sync_and_embed` and `embed_pending` use "embed everything" — the unsynced case.
- `remove_stale_vectors(live: &HashSet<ContentHash>, older_than)` removes, in the active
  profile, what decision 8 says.

### server: `SyncRuntime`

After a scan or a session has re-indexed a synced workspace, `embed(root)` runs one pass,
serialised per workspace like the re-index:

1. `refresh_embedder` (the bridge's model may have changed, #186).
2. The policy: read the replica's states once; a path may be embedded when its winner's
   replica is this replica's id, or when `is_primary(workspace_id)`.
3. `embed_pending_with(policy)`.
4. On the primary device, `remove_stale_vectors` with the winners' hashes.

A pass is also requested when the dial loop sees this host become a workspace's primary
device: files that arrived before the election settled would otherwise wait for the next
change. Failures are logged and never fail the session, like the re-index.

### sync: the filter

`SyncFilter::allows` refuses a conflict copy (`merge::conflict_path`'s
`.conflict-<id>-<n>` name) anywhere under `.<app>/embedded/`.

## Out of scope

- #188's scheduling, progress reporting and re-embedding after a model change beyond what
  falls out of decision 4.
- #206's second slot, query fallback between models, and the explicit prune command.
- Images and other non-text files.

## Testing

- **vectors:** a round trip through the format; a wrong dimension, a wrong profile or a
  truncated file reads as absent; the profile name changes with each of its five inputs.
- **retrieve:** with a source that finds a vector, nothing is embedded and the vector is
  stored; `may_embed = false` leaves the document pending; `embedded` is called once per
  computed vector; #194's tests keep passing with `NoVectorSource`.
- **workspace:** `embed_pending_with` writes a vector file and a second state over the same
  root picks it up without embedding; stale removal keeps live and young files.
- **sync:** the filter refuses a conflict copy under `.<app>/embedded/` and allows it
  elsewhere.
- **server (two hosts):** a file written on A is embedded on A only; B receives the vector
  and searches it semantically without embedding; a file written by a host with embedding
  off is embedded by the primary device.
