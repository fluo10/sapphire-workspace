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

1. **Three places, by owner.**
   - *Model settings* — provider, model, dimension, max_tokens, endpoint — live in the
     workgroup root and sync: `<bridge dir>/workgroups/<id>/root/embedding.toml`.
   - *Device settings* — whether this device embeds, and the local model's cache
     directory — live in `<bridge dir>/embedding.toml` and do not sync.
   - *The API key* lives in `<bridge dir>/embedding.key`, owner-only, and does not sync.
2. **A host without a workgroup** keeps its model settings in the device file, under a
   `[model]` table. When the workgroup's file exists, it wins and the device's `[model]`
   is ignored (status says so). `embedding set` writes the workgroup's file when the host
   belongs to one, and the device's `[model]` otherwise.
3. **No model settings anywhere means no embedding.** This is the state after a fresh
   install, as in #185. Removing the workgroup's file (`embedding clear`) turns embedding
   off for the whole workgroup.
4. **One device switch, `enabled`, with an automatic default.** Absent means *auto*: off
   when the provider is `local` and the CPU lacks AVX2 (x86_64 only; other architectures
   count as capable), on otherwise. `openai` is on by default even without AVX2, because
   the work happens elsewhere. An explicit `true` or `false` overrides auto. A device that
   is off answers `embed.info` with `enabled: false`, so its apps search with FTS only,
   and it never downloads the local model.
5. **Changes apply in place.** Neither the bridge nor the apps need a restart:
   - The bridge rebuilds its provider when the effective settings change — after a
     settings call, and when the synced model file changes (checked on every status tick,
     5 s). The old provider is dropped once its in-flight calls finish, which unloads a
     local model.
   - An app asks `embed.info` again on every `sync_and_embed`. A different model or
     dimension reconfigures the vector store (vectors of the old model are dropped and
     the documents become pending, as `configure_vectors` already does). A bridge that
     stopped embedding removes the app's embedder, and search falls back to FTS.
   - Between those two moments, `embed.embed` may answer with a model the app did not
     configure. `EmbedResult.model` already names it. The app's embedder treats a
     mismatch as an error, so a query falls back to FTS and the next sync reconfigures.
6. **The key is a secret.** It is read by the CLI from standard input or a no-echo prompt,
   never from an argument. It never appears in `status`, logs, tracing or `Debug`
   output; RPC results carry only `key_set: bool`. It travels once over the control
   plane, which is already restricted to this user (peer-uid check, DACL). `api_key_env`
   from #185 is removed: nothing has been released with it.
7. **Hybrid versus semantic search stays an application choice** (`HybridConfig`, the
   search mode of a query). It is not a device setting.

## Files

`<bridge dir>/workgroups/<id>/root/embedding.toml` (synced):

```toml
provider   = "local"                       # "local" | "openai"
model      = "Qwen/Qwen3-VL-Embedding-2B"
dimension  = 1024
max_tokens = 1024                          # local only
endpoint   = "https://api.openai.com"      # openai only
```

`<bridge dir>/embedding.toml` (this device):

```toml
enabled   = false            # absent: auto (off for local without AVX2)
cache_dir = "D:/models"      # local only, optional

[model]                      # only when the host has no workgroup
provider = "openai"
model    = "text-embedding-3-large"
dimension = 1024
endpoint = "http://127.0.0.1:11434"
```

`<bridge dir>/embedding.key` (this device): the key and nothing else, mode `0600` on Unix,
created inside the bridge directory's private ACL on Windows.

The #185 flat layout of `<bridge dir>/embedding.toml` was never released. A file in that
layout fails to parse; the bridge logs it and treats the device settings as absent.

## Types

The settings types move from `sapphire-framework-bridge-embed` to
`sapphire-framework-bridge-api`, as plain serde types with their validation. The bridge
library must read and write them for the RPCs below, and it does not depend on the
embedding crate.

```rust
pub enum Provider { Local, Openai }

pub struct ModelSettings {
    pub provider: Provider,
    pub model: String,
    pub dimension: u32,
    pub max_tokens: usize,
    pub endpoint: Option<String>,
}
impl ModelSettings { pub fn validate(&self) -> Result<(), String>; }   // #185's rules

pub struct DeviceSettings {
    pub enabled: Option<bool>,        // None = auto
    pub cache_dir: Option<PathBuf>,
}
```

`EmbedService::from_settings` takes `&ModelSettings`, the cache directory, and the key
(`Option<ApiKey>`, a newtype whose `Debug` prints `ApiKey(..)`).

The bridge's hook becomes reusable: `EmbedFactory` is an
`Arc<dyn Fn(&EmbedConfig) -> Option<Arc<dyn EmbedProvider>> + Send + Sync>`, where
`EmbedConfig` holds the effective model settings, cache directory and key. `Bridge.embed`
becomes swappable (`RwLock<Option<Arc<dyn EmbedProvider>>>`).

## Control plane (bridge-api 2.3.0, additive; `API_VERSION` stays 2)

| Method | Params | Result |
|---|---|---|
| `embed.settings` | — | `EmbedSettingsResult` |
| `embed.model_set` | `{ model: Option<ModelSettings> }` (`None` clears) | `EmbedSettingsResult` |
| `embed.device_set` | `{ enabled: Option<bool> }` | `EmbedSettingsResult` |
| `embed.key_set` | `{ key: String }` | `EmbedSettingsResult` |
| `embed.key_clear` | — | `EmbedSettingsResult` |

```rust
pub struct EmbedSettingsResult {
    pub model: Option<ModelSettings>,
    pub model_source: Option<ModelSource>,   // Workgroup | Device
    pub shadowed_device_model: bool,         // the device's [model] is ignored
    pub device: DeviceSettings,
    pub enabled: bool,                       // the effective switch
    pub avx2: bool,
    pub key_set: bool,
    pub info: EmbedInfoResult,               // what embed.info answers now
}
```

`EmbedInfoResult` gains `#[serde(default)] note: Option<EmbedNote>`: why it is off, or
what is missing — `NotConfigured`, `DisabledOnDevice`, `NoAvx2`, `KeyMissing` (openai
without a key; the call is still attempted, since a local endpoint may need none),
`Invalid(String)`. `bridge.status` carries it through `embedding` as it does today.

`embed.model_set` validates before it writes. It writes atomically (temporary file and
rename); the workgroup's file then syncs like any other change to the root.

## CLI

On `sapphire-bridge`, and on every app through `FrameworkCommand` (as with `device` and
`workgroup`, the app's CLI goes to the bridge):

```
embedding show
embedding set --provider <local|openai> [--model M] [--dimension N]
              [--max-tokens N] [--endpoint URL]
embedding clear                      # remove the model settings: embedding off
embedding device <auto|on|off>       # this device
embedding key set                    # stdin, or a no-echo prompt on a terminal
embedding key clear
```

`set` replaces the model settings: options not given take their defaults, so the result
never mixes an old endpoint with a new provider. When the bridge is not running, the CLI
writes the files directly, so a device can be prepared before its bridge starts; a
running bridge picks the change up as in decision 5.

`show` prints the effective settings, their source, the device switch with the reason
(`auto: off, no AVX2`), whether a key is set, and the model's state (`loaded`,
`unloaded`, or the note).

## GUI

The sync panel gains an **Embedding** screen beside Devices and Workspaces:

- A status line: the model and whether it is loaded, or why embedding is off; a warning
  when the provider is `openai` and no key is set.
- **Workgroup model** (or **Model** without a workgroup): provider, model, dimension,
  max_tokens (local), endpoint (openai), with Save and Clear. A note when the device's own
  `[model]` is shadowed.
- **This device**: Auto / On / Off, showing what Auto resolves to and why.
- **API key**: a password field with Set, and Clear; it shows only whether a key is set.

The view is a pure renderer over the snapshot, like the others; the snapshot gains the
`EmbedSettingsResult`, refreshed with the bridge's status.

## Changing the model

Vectors of the old model become useless. Each app reconfigures its store on its next
sync and re-embeds what it holds locally. Once vectors sync (#187), re-embedding the
workgroup's documents is the primary device's backfill (#188); this issue adds nothing
for it.

## Out of scope

- Syncing vectors (#187) and backfill (#188).
- A configurable REST timeout and `HF_ENDPOINT` (the #185 leftovers). They fit
  `ModelSettings` and `DeviceSettings` later without changing this design.
- An OS keyring for the key.

## Testing

- **bridge-api:** settings round-trip; `validate` keeps #185's rules; old results without
  `note` still parse.
- **bridge:** effective settings — workgroup wins over the device's `[model]`, auto with
  and without AVX2 (the detection is injected), `openai` stays on without AVX2; each
  `embed.*` settings method writes the right file; the key file is `0600` and the key is
  absent from status and `Debug`; a changed workgroup file rebuilds the provider within a
  tick; clearing the model makes `embed.info` answer `enabled: false`.
- **workspace:** `sync_and_embed` reconfigures after the bridge changes model; a bridge
  that turns embedding off removes the embedder; a mismatched `EmbedResult.model` is an
  error and search falls back to FTS.
- **CLI:** `key set` reads stdin; `set` replaces rather than merges; offline writes reach
  the files.
- **GUI:** the model view renders each state (`model.rs` tests, as for the device list).
