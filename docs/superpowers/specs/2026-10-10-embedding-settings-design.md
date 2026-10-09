# Embedding settings: shared per workgroup, switched per device, with a CLI and a GUI

Issue: #186. Builds on #185 (embedding moved to the bridge, spec
`2026-10-09-bridge-embedding-design.md`). Related: #183 (spike), #187 (synced vectors),
#188 (backfill on the primary device).

## Why

#185 put every embedding setting in one per-device file, `<bridge dir>/embedding.toml`,
edited by hand. That was a stopgap:

- The **model** must be the same on every device of a workgroup. Vectors from different
  models or dimensions cannot be compared, and #187 will sync them. Setting it device by
  device invites drift.
- The **API key** must not be synced. It is a secret of this device, and the workgroup
  root is copied to every device.
- Whether a device **embeds at all** is a property of the device. #183 measured the local
  2B model on a CPU without AVX at about 203 s for 700 tokens. Such a device can neither
  index documents nor embed a search query in useful time — a query of a few dozen
  tokens with its template is still 10 s or more, after loading 4 GB of weights. So one
  switch covers both, and synced vectors (#187) do not change that: a nearest-neighbour
  search needs the query as a vector too.
- Nobody should have to edit TOML. The bridge CLI and the sync GUI set all of it.

## Decisions

1. **Two model slots: one local, one remote.** A workgroup configures at most one
   *local* model (computed on the device's CPU) and at most one *remote* model (an
   OpenAI-compatible endpoint). The slot decides the provider, so there is no `provider`
   field. A list of models was considered and rejected: it needs an explicit priority and
   a per-entry switch on every device, and the one case it serves beyond two slots — a
   bigger local model on a stronger machine — is not worth that. With two slots the
   priority is implicit: **remote when it can be reached, local otherwise.**
2. **Three places, by owner.**
   - *Model settings* — the two slots — live in the workgroup root and sync:
     `<bridge dir>/workgroups/<id>/root/embedding.toml`.
   - *Device settings* — a switch per slot, and the local model's cache directory — live
     in `<bridge dir>/embedding.toml` and do not sync.
   - *The API key* of the remote slot lives in `<bridge dir>/embedding.key`, owner-only,
     and does not sync.
3. **A host without a workgroup** keeps its model settings in the device file, under a
   `[models]` table of the same shape. When the workgroup's file exists, it wins and the
   device's `[models]` is ignored (status says so). `embedding … set` writes the
   workgroup's file when the host belongs to one, and the device's `[models]` otherwise.
4. **No slot configured means no embedding.** This is the state after a fresh install, as
   in #185. Clearing both slots turns embedding off for the whole workgroup.
5. **One switch per slot on each device, with an automatic default.** Absent means
   *auto*: the local slot is off when the CPU lacks AVX2 (x86_64 only; other
   architectures count as capable), the remote slot is on. An explicit `true` or `false`
   overrides auto. #183 measured the local 2B model without AVX at about 203 s for 700
   tokens: such a device can neither index nor embed a query in useful time, so a slot
   that is off is off for both, and synced vectors (#187) do not change that. A device
   with no usable slot answers `embed.info` with `enabled: false`, so its apps search
   with FTS only, and it never downloads the local model.
6. **What #186 serves: one model.** Until #206 lets a device hold vectors
   of both models and fall back between them, the bridge serves a single *active*
   model: the remote slot when it is configured and on for this device, the local slot
   otherwise. The choice is static — it does not follow reachability yet — so a device
   does not flip its vector store between models when the network comes and goes.
7. **Changes apply in place.** Neither the bridge nor the apps need a restart:
   - The bridge rebuilds its provider when the effective settings change — after a
     settings call, and when the synced model file changes (checked on every status tick,
     5 s). The old provider is dropped once its in-flight calls finish, which unloads a
     local model.
   - An app asks `embed.info` again on every `sync_and_embed`. A different active model
     or dimension reconfigures the vector store, as `configure_vectors` does today. A
     bridge that stopped embedding removes the app's embedder, and search falls back to
     FTS.
   - Between those two moments, `embed.embed` may answer with a model the app did not
     configure. `EmbedResult.model` already names it. The app's embedder treats a
     mismatch as an error, so a query falls back to FTS and the next sync reconfigures.
8. **The key is a secret.** It is read by the CLI from standard input or a no-echo prompt,
   never from an argument. It never appears in `status`, logs, tracing or `Debug`
   output; RPC results carry only `key_set: bool`. It travels once over the control
   plane, which is already restricted to this user (peer-uid check, DACL). `api_key_env`
   from #185 is removed: nothing has been released with it.
9. **Hybrid versus semantic search stays an application choice** (`HybridConfig`, the
   search mode of a query). It is not a device setting.

## Files

`<bridge dir>/workgroups/<id>/root/embedding.toml` (synced). Either table may be absent:

```toml
[local]
model      = "Qwen/Qwen3-VL-Embedding-2B"   # the only local model for now
dimension  = 1024
max_tokens = 1024

[remote]
endpoint  = "https://api.openai.com"
model     = "text-embedding-3-large"
dimension = 1024
```

`<bridge dir>/embedding.toml` (this device):

```toml
cache_dir = "D:/models"      # local model files, optional

[local]
enabled = false              # absent: auto (off without AVX2)

[remote]
enabled = true               # absent: auto (on)

[models.remote]              # only when the host has no workgroup; same shape as above
endpoint  = "http://127.0.0.1:11434"
model     = "nomic-embed-text"
dimension = 768
```

`<bridge dir>/embedding.key` (this device): the remote slot's key and nothing else, mode
`0600` on Unix, created inside the bridge directory's private ACL on Windows.

The #185 flat layout of `<bridge dir>/embedding.toml` was never released. A file in that
layout fails to parse; the bridge logs it and treats the device settings as absent.

## Types

The settings types move from `sapphire-framework-bridge-embed` to
`sapphire-framework-bridge-api`, as plain serde types with their validation. The bridge
library must read and write them for the RPCs below, and it does not depend on the
embedding crate.

```rust
pub enum Slot { Local, Remote }

pub struct LocalModel  { pub model: String, pub dimension: u32, pub max_tokens: usize }
pub struct RemoteModel { pub endpoint: String, pub model: String, pub dimension: u32 }

pub struct ModelSettings {                // the synced file
    pub local: Option<LocalModel>,
    pub remote: Option<RemoteModel>,
}
impl ModelSettings { pub fn validate(&self) -> Result<(), String>; }   // #185's rules

pub struct DeviceSettings {
    pub local_enabled: Option<bool>,      // None = auto
    pub remote_enabled: Option<bool>,
    pub cache_dir: Option<PathBuf>,
}
```

`EmbedService` gains two constructors, `local(&LocalModel, cache_dir)` and
`remote(&RemoteModel, Option<ApiKey>)`; `ApiKey` is a newtype whose `Debug` prints
`ApiKey(..)`.

The bridge's hook becomes reusable: `EmbedFactory` is an
`Arc<dyn Fn(&EmbedConfig) -> Option<Arc<dyn EmbedProvider>> + Send + Sync>`, where
`EmbedConfig` names the active slot with its model, the cache directory and the key.
`Bridge.embed` becomes swappable (`RwLock<Option<Arc<dyn EmbedProvider>>>`). #206
issue turns it into one provider per slot.

## Control plane (bridge-api 2.3.0, additive; `API_VERSION` stays 2)

| Method | Params | Result |
|---|---|---|
| `embed.settings` | — | `EmbedSettingsResult` |
| `embed.model_set` | `{ slot: Slot, model: Option<LocalModel / RemoteModel> }` (`None` clears) | `EmbedSettingsResult` |
| `embed.device_set` | `{ slot: Slot, enabled: Option<bool> }` | `EmbedSettingsResult` |
| `embed.key_set` | `{ key: String }` | `EmbedSettingsResult` |
| `embed.key_clear` | — | `EmbedSettingsResult` |

```rust
pub struct EmbedSettingsResult {
    pub models: ModelSettings,
    pub source: Option<ModelSource>,         // Workgroup | Device
    pub shadowed_device_models: bool,        // the device's [models] is ignored
    pub device: DeviceSettings,
    pub local_enabled: bool,                 // the effective switches
    pub remote_enabled: bool,
    pub active: Option<Slot>,
    pub avx2: bool,
    pub key_set: bool,
    pub info: EmbedInfoResult,               // what embed.info answers now
}
```

`embed.model_set` carries the slot's model as a tagged value so one method serves both
slots. It validates before it writes, and writes atomically (temporary file and rename);
the workgroup's file then syncs like any other change to the root.

`EmbedInfoResult` gains `#[serde(default)] note: Option<EmbedNote>`: why it is off, or
what is missing — `NotConfigured`, `DisabledOnDevice`, `NoAvx2`, `KeyMissing` (remote
without a key; the call is still attempted, since a local endpoint may need none),
`Invalid(String)`. `bridge.status` carries it through `embedding` as it does today.

## CLI

On `sapphire-bridge`, and on every app through `FrameworkCommand` (as with `device` and
`workgroup`, the app's CLI goes to the bridge):

```
embedding show
embedding local set [--dimension N] [--max-tokens N]
embedding remote set --endpoint URL --model M --dimension N
embedding <local|remote> clear           # remove that slot for the workgroup
embedding <local|remote> device <auto|on|off>   # this device's switch
embedding key set                        # stdin, or a no-echo prompt on a terminal
embedding key clear
```

`set` replaces the slot: options not given take their defaults, so the result never mixes
an old endpoint with a new model. When the bridge is not running, the bridge's own CLI
writes the files directly, so a device can be prepared before its bridge starts; a running
bridge picks the change up as in decision 7. An app's CLI only talks to a running bridge:
the files are the bridge's, and the app does not link the code that writes them.

`show` prints both slots, their source, each switch with the reason (`auto: off, no
AVX2`), which slot is active, whether a key is set, and the model's state (`loaded`,
`unloaded`, or the note).

## GUI

The sync panel gains an **Embedding** screen beside Devices and Workspaces:

- A status line: the active model and whether it is loaded, or why embedding is off; a
  warning when the remote slot is active and no key is set.
- **Local model** and **Remote model**, one section each (titled for the workgroup when
  the host has one): the slot's fields with Save and Clear, and this device's
  Auto / On / Off, showing what Auto resolves to and why. A note when the device's own
  `[models]` is shadowed.
- **API key** under the remote model: a password field with Set, and Clear; it shows only
  whether a key is set.

The view is a pure renderer over the snapshot, like the others; the snapshot gains the
`EmbedSettingsResult`, refreshed with the bridge's status.

## Changing the model

In #186 a change of the active model reconfigures each app's store on its next sync, which
drops the old model's vectors and re-embeds what the app holds locally — the behaviour
`configure_vectors` has today.

#206 changes that: vectors are kept per model, a file holds vectors of both
slots, search falls back from remote to local when the endpoint cannot be reached, and
vectors of a model no slot names any more are removed only by an explicit command, so a
model can be tried and then reverted without re-embedding. Once vectors sync (#187),
re-embedding the workgroup's documents is the primary device's backfill (#188).

## Out of scope

- Serving both slots at once, falling back between them, keeping vectors per model and
  removing unused ones explicitly: #206.
- Syncing vectors (#187) and backfill (#188).
- A configurable REST timeout and `HF_ENDPOINT` (the #185 leftovers). They fit
  `ModelSettings` and `DeviceSettings` later without changing this design.
- An OS keyring for the key.

## Testing

- **bridge-api:** settings round-trip; `validate` keeps #185's rules; old results without
  `note` still parse.
- **bridge:** effective settings — workgroup wins over the device's `[models]`, auto with
  and without AVX2 (the detection is injected), the remote slot stays on without AVX2,
  the remote slot is active over the local one when both are on; each
  `embed.*` settings method writes the right file; the key file is `0600` and the key is
  absent from status and `Debug`; a changed workgroup file rebuilds the provider within a
  tick; clearing the model makes `embed.info` answer `enabled: false`.
- **workspace:** `sync_and_embed` reconfigures after the bridge changes model; a bridge
  that turns embedding off removes the embedder; a mismatched `EmbedResult.model` is an
  error and search falls back to FTS.
- **CLI:** `key set` reads stdin; `<slot> set` replaces rather than merges; offline writes reach
  the files.
- **GUI:** the model view renders each state (`model.rs` tests, as for the device list).
