# Embedding settings — implementation plan (#186)

**Spec:** `docs/superpowers/specs/2026-10-10-embedding-settings-design.md`
**Branch:** `feat/issue-186-embedding-settings`
**Follow-up:** #206 (both slots at once, per-model vectors, explicit pruning)

Each task ends green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
-D warnings`, and the tests of the crates it touches. The last task runs the whole suite.

## Task 1 — bridge-api 2.3.0: settings types, methods, client, CLI pieces

`crates/sapphire-framework-bridge-api`

- `src/embed.rs` (new, re-exported from `lib.rs`):
  - `Slot { Local, Remote }` (serde lowercase).
  - `LocalModel { model, dimension, max_tokens }` with `Default` (#185's defaults),
    `RemoteModel { endpoint, model, dimension }`; both `deny_unknown_fields`.
  - `ModelSettings { local: Option<LocalModel>, remote: Option<RemoteModel> }` with
    `validate()` (#185's local rules; remote: `http(s)://` endpoint, non-empty model,
    dimension ≥ 1) and `is_empty()`.
  - `DeviceSettings { local_enabled, remote_enabled: Option<bool>, cache_dir }`.
  - `ApiKey(String)`: `#[serde(transparent)]`, `Debug` prints `ApiKey(..)`, `expose()`.
  - `ModelSource { Workgroup, Device }`, `EmbedNote { NotConfigured, DisabledOnDevice,
    NoAvx2, KeyMissing, Invalid(String) }` with `Display`.
  - `EmbedSettingsResult` as in the spec.
  - Params: `EmbedModelSetParams { slot, model: Option<SlotModel> }` where
    `SlotModel` is `#[serde(untagged)]`-free: `enum SlotModel { Local(LocalModel),
    Remote(RemoteModel) }` (externally tagged), checked against `slot`;
    `EmbedDeviceSetParams { slot, enabled: Option<bool> }`;
    `EmbedKeySetParams { key: ApiKey }` (redacted `Debug` through `ApiKey`).
- `EmbedInfoResult` gains `#[serde(default, skip_serializing_if)] note: Option<EmbedNote>`.
- Method names: `EMBED_SETTINGS`, `EMBED_MODEL_SET`, `EMBED_DEVICE_SET`, `EMBED_KEY_SET`,
  `EMBED_KEY_CLEAR`; client calls of the same names.
- `describe(&EmbedSettingsResult) -> Vec<String>`: the lines `embedding show` prints
  (both CLIs share it).
- Feature `cli` (clap + rpassword): `EmbeddingCommand` (the spec's subcommands),
  `read_key()` (stdin when not a terminal, else a no-echo prompt), and
  `EmbeddingCommand::run_online(&BridgeClient) -> Result<EmbedSettingsResult>`.
- Version 2.3.0; workspace's dependency on it to `2.3.0`.
- Tests: round-trips, `validate`, an `EmbedInfoResult` without `note` parses,
  `ApiKey` and `EmbedKeySetParams` `Debug` hide the key, `describe` per state.

## Task 2 — bridge-embed: build from a slot, key passed in

`crates/sapphire-framework-bridge-embed`

- Remove `settings.rs` (`EmbeddingSettings`, `Provider`); keep `LOCAL_MODEL` and
  `MAX_TOKENS` as re-exports from bridge-api's validation constants (move them there).
- `EmbedService::local(&LocalModel, cache_dir: PathBuf)` and
  `EmbedService::remote(&RemoteModel, Option<ApiKey>)`.
- `RestEmbedder::new(&RemoteModel, Option<ApiKey>)`: no environment lookup; no
  `Authorization` header without a key. `LocalQwen::load(&LocalModel, &Path)`.
- Tests adapted; a REST test pins that the key reaches the header and is absent without one.

## Task 3 — bridge: settings files and resolution

`crates/sapphire-framework-bridge/src/embed_settings.rs` (new)

- Paths: `BridgeDir::embedding_toml()`, `BridgeDir::embedding_key()`,
  `Workgroup::embedding_toml()` (`<wg dir>/root/embedding.toml`).
- `DeviceFile { cache_dir, local: Switch, remote: Switch, models: Option<ModelSettings> }`
  (`Switch { enabled: Option<bool> }`), load (absent → default; unparsable → logged,
  default) and save.
- Shared file load/save; key load/save/clear (`0600` on Unix; atomic writes through a
  temporary file and rename, as `status.json` does).
- `avx2()` (x86_64: `is_x86_feature_detected!("avx2")`; elsewhere `true`).
- `resolve(shared: Option<Result<ModelSettings>>, device: &DeviceFile, key_set: bool,
  avx2: bool) -> Resolved` — pure: models, source, shadowed, effective switches, active
  slot, note.
- `EmbedConfig { Local { model, cache_dir }, Remote { model, key } }` (`PartialEq`,
  redacted `Debug`), built from `Resolved` plus the key.
- `set_model(dir, slot, Option<SlotModel>)`, `set_device(dir, slot, Option<bool>)`,
  `set_key`, `clear_key`, `settings(dir, avx2) -> (EmbedSettingsResult without info,
  Option<EmbedConfig>, Option<EmbedNote>)` — the functions the RPCs and the offline CLI
  share.
- Tests: the spec's resolution cases; writes land in the workgroup's file when there is
  one; key file mode; `Debug` of `EmbedConfig` hides the key.

## Task 4 — bridge: reloadable provider, RPCs, the binary

- `EmbedFactory` becomes `Arc<dyn Fn(&EmbedConfig, &BridgeDir) -> Option<Arc<dyn
  EmbedProvider>> + Send + Sync>`. `Bridge` gains `embed_factory`, `embed:
  RwLock<Option<Arc<dyn EmbedProvider>>>`, `embed_applied: Mutex<Option<EmbedConfig>>`,
  `embed_note: Mutex<Option<EmbedNote>>`, `avx2: bool` (builder `avx2()` for tests).
  `embed_provider()` stays: a fixed provider, never replaced (tests, in-process hosts that
  manage their own).
- `Bridge::reload_embed()`: resolve; rebuild only when the config differs from the
  applied one. Called before the loops start, after every settings RPC, and every
  `STATUS_INTERVAL` by a loop in `serve_loops`.
- `control.rs`: the five methods; `embed_info` reports `note`.
- `command.rs`: `dispatch_with` takes the new factory and passes it to the bridge instead
  of building a provider once.
- `apps/sapphire-bridge/src/embed.rs`: the factory maps `EmbedConfig` to
  `EmbedService::local`/`remote`.
- Tests: a factory that counts builds — an unchanged config does not rebuild, a changed
  workgroup file does within a tick; each RPC; `status` never carries the key.

## Task 5 — CLIs

- Bridge: `BridgeCommand::Embedding(EmbeddingCommand)`; online through the client,
  offline through `embed_settings` on the bridge directory; prints `describe`.
- Server: `FrameworkCommand::Embedding(EmbeddingCommand)`; online only (the app's CLI
  never writes the bridge's files), "the bridge is not running" otherwise.
- Tests: offline `set` writes the device file without a workgroup; `set` replaces.

## Task 6 — workspace: follow the bridge's model

`crates/sapphire-framework-workspace`

- `BridgeEmbedder`: a `Job::Info` asks `embed.info` on the existing connection;
  `embed_texts` rejects an `EmbedResult` whose model or dimension differ from its own.
- `WorkspaceState.embedder`: `RwLock<Option<Arc<BridgeEmbedder-or-test-embedder>>>` plus
  a "probed" flag; `embedder()` returns `Option<Arc<dyn Embedder + Send + Sync>>`.
  `load_embedder*` keep their probe-once meaning for search; `sync_and_embed` calls
  `refresh_embedder_async()`, which re-asks and, on a different model or dimension,
  reconfigures the vector store and swaps; on `enabled: false` or no bridge it removes
  the embedder.
- Tests (FakeBridge): a model change reconfigures; disabling removes; a mismatched
  result is an error.

## Task 7 — GUI

`crates/sapphire-framework-gui`

- `BridgeState` gains `embedding: Option<EmbedSettingsResult>` (an older bridge without
  the method: `None`, and the screen says the bridge is too old).
- Commands `EmbedModelSet`, `EmbedDeviceSet`, `EmbedKeySet`, `EmbedKeyClear`.
- `views/embedding.rs`: the spec's screen; pure helpers in `model.rs` (status text,
  what Auto resolves to) with tests. `panel.rs`: a new `Screen`.

## Task 8 — docs and the whole suite

- ARCHITECTURE (bridge role 4), CHANGELOG (replace #185's `<bridge dir>/embedding.toml`
  and `api_key_env` text; bridge-api 2.3.0), the bridge-embedding spec's settings section
  points to the new spec.
- `cargo test --workspace --no-fail-fast`.
