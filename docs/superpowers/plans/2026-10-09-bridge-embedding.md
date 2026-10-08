# Embedding in the Bridge Implementation Plan (#185, #194)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The bridge embeds text for every app on the host. It holds one Qwen3-VL-Embedding-2B model, or uses an OpenAI-compatible endpoint. Apps ask for embeddings over IPC and no longer link fastembed.

**Architecture:**
- **bridge-api.** `sapphire-framework-bridge-api` gains `embed.info` / `embed.embed`.
- **bridge library.** `sapphire-framework-bridge` serves those methods through an injectable `EmbedProvider` and does not depend on any model code.
- **embed crate.** A new bridge-only crate, `apps/sapphire-bridge-embed`, implements the local and REST providers behind a single-worker `EmbedService`.
- **bridge binary.** The `apps/sapphire-bridge` binary reads `embedding.toml` and injects the service.
- **retrieve.** `sapphire-framework-retrieve` keeps only the `Embedder` trait. Its store learns `configure_vectors(model, dim)`, which replaces reopening with a dimension (#195).
- **workspace.** `sapphire-framework-workspace` gets a thread-owning `BridgeEmbedder` and drops app-side embedding config.

**Tech Stack:** Rust 2024, tokio, fastembed 7.1.1 (`qwen3` + `ort-load-dynamic`), candle (via fastembed), tokenizers, ureq 3, serde/toml, sapphire-ipc.

**Spec:** `docs/superpowers/specs/2026-10-09-bridge-embedding-design.md`

## Global Constraints

- **Crate naming and dependency direction.**
  - The new crate is `sapphire-bridge-embed` at `apps/sapphire-bridge-embed/`.
  - Only `apps/sapphire-bridge` depends on it.
  - `sapphire-framework-bridge`, which the facade re-exports, must not depend on it, directly or through a feature.
- **bridge-api.** `sapphire-framework-bridge-api` version is `2.2.0`. `API_VERSION` stays `2`. Every new field is `#[serde(default)]`.
- **Method names.** `EMBED_INFO = "embed.info"`, `EMBED = "embed.embed"`.
- **Local model.**
  - Model: `Qwen/Qwen3-VL-Embedding-2B`, at f32.
  - Template:
    `<|im_start|>system\nRepresent the user's input.<|im_end|>\n<|im_start|>user\n{content}<|im_end|>\n<|im_start|>assistant\n`
  - `TEMPLATE_VERSION = 1`.
  - Content is truncated by tokens before templating. The model's `max_length` is `max_tokens + 64`.
- **Settings defaults.** `dimension = 1024` (MRL: keep the first N values, then L2-normalize again), `max_tokens = 1024`. `dimension` must be in `64..=2048` for local.
- **REST limits.** `MAX_INPUT_CHARS = 4_000`, `MAX_REQUEST_CHARS = 100_000`. The response must hold exactly one vector per input.
- **Service timing.** `IDLE_UNLOAD = 10 min`, `LOAD_RETRY_BACKOFF = 60 s`. Inference batches inside the worker are 32 texts.
- **Settings file.** `<bridge dir>/embedding.toml`. A missing file means disabled.
- **embed_pending (#194).** When every per-item retry in a batch fails, stop. The remaining documents stay pending and the error is returned.
- **Tests on this host.** The CPU has no AVX.
  - Before Task 5 removes fastembed from retrieve, run retrieve and workspace tests with `--no-default-features --features redb-store`. After Task 5, run them normally.
  - The embed crate uses `ort-load-dynamic`, so it is safe here.
  - Tests that load the real model are `#[ignore]`.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A provider outage must not become a request storm.** If the bridge is down or the endpoint fails for everything, `embed_pending` stops after one fully failed batch. Pinned in Task 5.
2. **`BridgeEmbedder` called from inside a tokio runtime must not panic or deadlock.** The server calls into the workspace from runtime threads and from `spawn_blocking`. Pinned in Task 6.
3. **Changing the model or dimension must not mix vectors.** The store drops its vectors and re-embeds. Pinned in Task 5.
4. **Japanese content at the truncation boundary.** Truncation must not panic, and the template must survive. Pinned in Task 3.
5. **The facade build must not pull in fastembed or candle.** `cargo tree -p sapphire-framework -e normal -i fastembed` must be empty. Pinned in Task 7.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/sapphire-framework-bridge-api/{Cargo.toml,src/lib.rs,src/client.rs}` | modify | 2.2.0; embed method types; client methods |
| `crates/sapphire-framework-bridge/src/embed.rs` | **create** | `EmbedProvider` trait, `EmbedStatus` |
| `crates/sapphire-framework-bridge/src/{lib.rs,control.rs,status.rs,command.rs}` | modify | builder, the two methods, status line, `dispatch_with` |
| `apps/sapphire-bridge-embed/` | **create** | settings, template/truncate/MRL helpers, REST provider, local provider, `EmbedService` |
| `apps/sapphire-bridge/{Cargo.toml,src/main.rs}` | modify | read settings, start service, inject |
| `Cargo.toml` (workspace) | modify | add member `apps/sapphire-bridge-embed` |
| `crates/sapphire-framework-retrieve/` | modify | trait-only `embed.rs`; config without embedding; `configure_vectors`; #194 stop rule |
| `crates/sapphire-framework-workspace/src/bridge_embedder.rs` | **create** | `BridgeEmbedder` |
| `crates/sapphire-framework-workspace/src/{workspace_state.rs,lib.rs,config.rs,Cargo.toml}` | modify | `load_embedder()` via the bridge; drop embedding config and the fastembed feature |
| `crates/sapphire-framework/`, `crates/sapphire-framework-backend/` | modify | compile fallout (re-exports, `RetrieveConfig`) |
| `CHANGELOG.md`, `docs/ARCHITECTURE.md` | modify | docs |

---

### Task 1: bridge-api 2.2.0 — embed method types and client

**Files:** `crates/sapphire-framework-bridge-api/Cargo.toml` (version `2.2.0`), `src/lib.rs`, `src/client.rs`

**Produces:**
```rust
pub const EMBED_INFO: &str = "embed.info";
pub const EMBED: &str = "embed.embed";
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EmbedModelInfo { pub model: String, pub dimension: u32, pub template_version: u32 }
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EmbedInfoResult {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub model: Option<EmbedModelInfo>,
    /// Whether the model is in memory right now (local provider); always true for REST.
    #[serde(default)] pub loaded: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmbedParams { pub texts: Vec<String> }
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmbedResult { pub model: String, pub dimension: u32, pub vectors: Vec<Vec<f32>> }
// StatusResult gains:
#[serde(default, skip_serializing_if = "Option::is_none")] pub embedding: Option<EmbedInfoResult>,
// BridgeClient:
pub async fn embed_info(&self) -> sapphire_ipc::Result<EmbedInfoResult>;
pub async fn embed(&self, texts: Vec<String>) -> sapphire_ipc::Result<EmbedResult>;
```
The spec's flat `model`/`dimension`/`template_version` options are grouped into `EmbedModelInfo`, which carries the same information.

- [ ] **Step 1: Failing tests** (in `lib.rs`'s tests):
  - `EmbedInfoResult` round-trips through serde.
  - A 2.0-shaped `StatusResult` JSON without `embedding` deserializes to `embedding: None`.
  - `EmbedParams` serializes as `{"texts":[...]}`.
- [ ] **Step 2: Run them.** `cargo test -p sapphire-framework-bridge-api` → compile errors.
- [ ] **Step 3: Implement.** Copy the client methods' shape from `device_retire` / `peers`: one `self.client.call(EMBED, EmbedParams { texts })`. Fix every `StatusResult { .. }` literal in the workspace by adding `embedding: None`. Find them with `cargo check --workspace --all-targets`.
- [ ] **Step 4: Run.** `cargo test -p sapphire-framework-bridge-api && cargo check --workspace --all-targets`.
- [ ] **Step 5: Commit.** `feat(bridge-api): 2.2.0 — embed.info and embed.embed` (Refs #185).

---

### Task 2: bridge library — `EmbedProvider` hook, the two methods, status

**Files:**
- Create `crates/sapphire-framework-bridge/src/embed.rs`.
- Modify `lib.rs` (field, builder, `pub use`), `control.rs` (router entries, handlers, `status` fills `embedding`), `command.rs` (`dispatch_with`).

**Consumes:** Task 1 types. **Produces:**
```rust
#[async_trait::async_trait]
pub trait EmbedProvider: Send + Sync {
    /// `None` when embedding is disabled on this host.
    fn info(&self) -> Option<EmbedModelInfo>;
    /// Whether the model is in memory now (REST providers: always true).
    fn loaded(&self) -> bool;
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String>;
}
impl Bridge { pub fn embed_provider(mut self, p: Arc<dyn EmbedProvider>) -> Bridge; }
/// Builds the provider once the bridge directory is known (the binary reads embedding.toml).
pub type EmbedFactory = Box<dyn FnOnce(&BridgeDir) -> Option<Arc<dyn EmbedProvider>> + Send>;
impl BridgeCommand {
    pub async fn dispatch(self, version: &'static str) -> Result<i32>;            // = dispatch_with(version, None)
    pub async fn dispatch_with(self, version: &'static str, embed: Option<EmbedFactory>) -> Result<i32>;
}
```

- [ ] **Step 1: Failing tests** (in `control.rs` tests, reusing `bridge()`, `connect()` and `call()`). Add a fake provider:
```rust
struct FakeProvider;
#[async_trait::async_trait]
impl crate::EmbedProvider for FakeProvider {
    fn info(&self) -> Option<sapphire_bridge_api::EmbedModelInfo> {
        Some(sapphire_bridge_api::EmbedModelInfo { model: "fake".into(), dimension: 2, template_version: 1 })
    }
    fn loaded(&self) -> bool { true }
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        if texts.iter().any(|t| t == "boom") { return Err("model failed".into()); }
        Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
    }
}
```
  Tests:
  - `embed_info_without_a_provider_is_disabled`
  - `embed_without_a_provider_is_an_error`
  - `embed_info_reports_the_provider_model`
  - `embed_returns_one_vector_per_text_in_order`
  - `a_provider_error_is_an_rpc_error_with_its_message`
  - `status_reports_embedding`
- [ ] **Step 2: Run them.** `cargo test -p sapphire-framework-bridge embed` → compile errors.
- [ ] **Step 3: Implement.**
  - The `Bridge` field is `embed: Option<Arc<dyn EmbedProvider>>` and defaults to `None`.
  - Handlers:
    - `embed.info` answers `EmbedInfoResult { enabled: info.is_some(), model: info, loaded }`.
    - `embed.embed` answers `EmbedResult { model, dimension, vectors }`. With no provider, or with `info()` returning `None`, it returns `Error::Config("embedding is not enabled on this host")`. A provider `Err(msg)` becomes a failed RPC carrying `msg`.
  - Register both in `router`, in the same shape as `PEERS`.
  - In `command.rs`, the `Serve` path is `run(version)`. Thread the factory through `run(version, embed)`, call it with `&dir` after `build_bridge`, and apply `.embed_provider(p)` when it returns `Some`. `dispatch` delegates to `dispatch_with(version, None)`.
- [ ] **Step 4: Run.** `cargo test -p sapphire-framework-bridge`. Then confirm that `cargo tree -p sapphire-framework-bridge -i candle-core` finds nothing.
- [ ] **Step 5: Commit.** `feat(bridge): an EmbedProvider hook and the embed.* control methods` (Refs #185).

---

### Task 3: `apps/sapphire-bridge-embed` — settings, helpers, REST, local, service

**Files:**
- Create `apps/sapphire-bridge-embed/Cargo.toml` and `src/{lib.rs,settings.rs,template.rs,rest.rs,local.rs,service.rs}`.
- Add the crate as a member of the root `Cargo.toml`.

`Cargo.toml`:
```toml
[package]
name = "sapphire-bridge-embed"
version.workspace = true
edition.workspace = true
description = "Text embedding for sapphire-bridge: a local Qwen3-VL-Embedding model or an OpenAI-compatible endpoint"
license.workspace = true
repository.workspace = true
publish = false

[features]
default = ["local"]
# The local model. ORT is loaded dynamically (never at start), so CPUs without AVX still run.
local = ["dep:fastembed", "dep:candle-core", "dep:tokenizers"]

[dependencies]
fastembed = { version = "=7.1.1", default-features = false, features = ["qwen3", "hf-hub-native-tls", "ort-load-dynamic"], optional = true }
candle-core = { version = "0.11.0", optional = true }
tokenizers = { version = "0.23.2", default-features = false, features = ["onig"], optional = true }
ureq = { version = "3.3", features = ["json"] }
serde.workspace = true
serde_json.workspace = true
toml.workspace = true
thiserror.workspace = true
tracing.workspace = true
tokio = { workspace = true, features = ["sync", "rt"] }

[dev-dependencies]
tempfile = "3"
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
```
The versions match the #183 spike, which was verified on this host. Use what resolves if a patch level has moved.

**Produces:**
```rust
// settings.rs
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")] pub enum Provider { Local, Openai }
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EmbeddingSettings {
    #[serde(default)] pub enabled: bool,
    #[serde(default = "default_provider")] pub provider: Provider,
    #[serde(default = "default_model")] pub model: String,
    #[serde(default = "default_dimension")] pub dimension: u32,
    #[serde(default = "default_max_tokens")] pub max_tokens: usize,
    #[serde(default)] pub endpoint: Option<String>,
    #[serde(default)] pub api_key_env: Option<String>,
}
impl EmbeddingSettings {
    pub const FILE: &'static str = "embedding.toml";
    /// `Ok(None)` when the file is absent. Validates `dimension` (64..=2048) for local.
    pub fn load(path: &Path) -> Result<Option<Self>>;
}
// template.rs (pure)
pub const TEMPLATE_VERSION: u32 = 1;
pub fn wrap(content: &str) -> String;
/// `text` cut at the byte offset where token `max - 1` ends; `offsets[i] = (start, end)` per token.
pub fn truncate_at<'a>(text: &'a str, offsets: &[(usize, usize)], max: usize) -> &'a str;
/// First `dim` values, L2-normalized again. A zero vector stays zero.
pub fn mrl(v: &[f32], dim: usize) -> Vec<f32>;
// rest.rs
pub const MAX_INPUT_CHARS: usize = 4_000;
pub const MAX_REQUEST_CHARS: usize = 100_000;
pub fn cap_chars(text: &str, max: usize) -> &str;
/// Group input indices in order so each group's capped chars total ≤ MAX_REQUEST_CHARS;
/// an input over the cap alone forms its own group. Empty input → no groups.
pub fn request_groups(texts: &[&str]) -> Vec<std::ops::Range<usize>>;
pub trait HttpPost: Send + Sync { fn post_json(&self, url: &str, bearer: Option<&str>, body: serde_json::Value) -> Result<serde_json::Value>; }
pub struct RestEmbedder<H: HttpPost = UreqPost> { .. }
// service.rs
pub trait Embed: Send { fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>; }
pub type Loader = Box<dyn FnMut() -> Result<Box<dyn Embed>> + Send>;
pub struct EmbedService { .. }
impl EmbedService {
    pub fn from_settings(s: &EmbeddingSettings) -> Option<EmbedService>;   // None when disabled
    pub fn with_loader(info: ModelInfo, loader: Loader, idle_unload: Duration, retry_backoff: Duration) -> EmbedService; // tests
    pub fn info(&self) -> ModelInfo;
    pub fn loaded(&self) -> bool;
    pub async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;
}
pub struct ModelInfo { pub model: String, pub dimension: u32, pub template_version: u32 }
```

The worker design:
- A `std::thread` owns `Option<Box<dyn Embed>>`.
- Requests arrive on a `std::sync::mpsc::Sender<(Vec<String>, tokio::sync::oneshot::Sender<Result<..>>)>`.
- The worker waits with `recv_timeout(idle_unload)`. On a timeout it drops the model and clears the `loaded` flag (an `AtomicBool`).
- On a request, it loads the model if absent. If the last load failure is less than `retry_backoff` ago, it fails fast with the stored message. Otherwise it calls `loader()`.
- It embeds in chunks of 32 and replies.
- When `EmbedService` is dropped, the sender drops and the thread exits.

`local.rs` (feature `local`):
- `LocalQwen` implements `Embed`.
- `load(settings)` builds `fastembed::Qwen3TextEmbedding::from_hf("Qwen/Qwen3-VL-Embedding-2B", &Device::Cpu, DType::F32, max_tokens + 64)`. It loads the tokenizer from the hf-hub snapshot's `tokenizer.json`; the #183 spike located it under the cache's `snapshots/*/`. Use hf-hub's API through fastembed's re-export if one is available.
- `embed`: for each text, encode without special tokens, truncate with `truncate_at` using the encoding's offsets, and `wrap` the result. Call `model.embed`, then `mrl` each vector.

`RestEmbedder::embed`:
- Cap each input with `cap_chars`.
- Post each request group to `{endpoint}/v1/embeddings` with `{"model", "input": [...]}` and the bearer key read from `api_key_env` (default `OPENAI_API_KEY`).
- Require `data.len() == group.len()`. Order the results by `index`.
- Apply `mrl` when a vector is longer than `dimension`.

- [ ] **Step 1: Failing tests.** Run every test with `cargo test -p sapphire-bridge-embed`.
  - **settings:**
    - a missing file gives `None`;
    - a minimal `enabled = true` gets the defaults (local, the Qwen model, 1024, 1024);
    - `dimension = 4096` with local is an error.
  - **template:**
    - `wrap("x")` equals the exact template with `x`;
    - `truncate_at` with ASCII offsets cuts at the end of token `max - 1`;
    - a text under the limit is unchanged;
    - Japanese offsets give a valid char boundary;
    - `mrl` gives the expected length and a unit norm (within 1e-5), and a zero vector stays zero.
  - **rest:**
    - `request_groups` keeps order;
    - an over-cap single input forms its own group;
    - empty input gives no groups;
    - `RestEmbedder` with a fake `HttpPost` returning shuffled `index` values gives the original order;
    - a count mismatch is an error;
    - a 3000-dimension response with `dimension = 1024` returns 1024-length unit vectors.
  - **service**, with `with_loader` and a fake `Embed`:
    - requests are served in order;
    - the loader is called once across two requests;
    - with `idle_unload = 50ms`, `loaded()` becomes false after about 200 ms and the next request calls the loader again;
    - a failing loader fails the request, and a second request within `retry_backoff` fails without calling the loader again;
    - 70 texts are split into embed calls of at most 32 each.
  - **local**, `#[ignore]`: one real embedding gives 1024 values with norm ≈ 1. The sentences 「今日は雨が降っている」 and 「雨の日です」 are closer to each other than either is to 「請求書の支払い期限」.
- [ ] **Step 2: Run them** and confirm they fail.
- [ ] **Step 3: Implement** the modules described above.
- [ ] **Step 4: Run.** `cargo test -p sapphire-bridge-embed`. Run `cargo test -p sapphire-bridge-embed -- --ignored` once if the machine can (the spike host can; it downloads about 4 GB). Report whether it ran.
- [ ] **Step 5: Commit.** `feat(bridge-embed): local Qwen3-VL embedding and an OpenAI-compatible provider behind one worker` (Refs #185, #194).

---

### Task 4: bridge binary wiring

**Files:** `apps/sapphire-bridge/Cargo.toml` (dependency `sapphire-bridge-embed = { path = "../sapphire-bridge-embed" }`), `apps/sapphire-bridge/src/main.rs`

- [ ] **Step 1: Write the adapter and the factory.** They go in a new `apps/sapphire-bridge/src/embed.rs`, which `main.rs` includes with `mod embed;`.
```rust
pub struct ServiceProvider(sapphire_bridge_embed::EmbedService);
#[async_trait::async_trait]
impl sapphire_bridge::EmbedProvider for ServiceProvider {
    fn info(&self) -> Option<sapphire_bridge_api::EmbedModelInfo> { let i = self.0.info(); Some(EmbedModelInfo { model: i.model, dimension: i.dimension, template_version: i.template_version }) }
    fn loaded(&self) -> bool { self.0.loaded() }
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> { self.0.embed(texts).await.map_err(|e| e.to_string()) }
}
pub fn factory() -> sapphire_bridge::EmbedFactory {
    Box::new(|dir| {
        let path = dir.root().join(sapphire_bridge_embed::EmbeddingSettings::FILE);
        match sapphire_bridge_embed::EmbeddingSettings::load(&path) {
            Ok(Some(s)) => sapphire_bridge_embed::EmbedService::from_settings(&s).map(|svc| std::sync::Arc::new(ServiceProvider(svc)) as _),
            Ok(None) => None,
            Err(err) => { tracing::error!("embedding.toml: {err}; embedding is disabled"); None }
        }
    })
}
```
  - `BridgeDir` may name its root accessor differently from `dir.root()`; use whatever path accessor `BridgeDir` exposes. `embedding.toml` lives in the bridge directory itself.
  - Add `sapphire-bridge-api` (path dependency) and `async-trait` to the binary's dependencies if they are not already re-exported.
- [ ] **Step 2: Change `main`.** Replace `command.dispatch(..)` with `command.dispatch_with(env!("CARGO_PKG_VERSION"), Some(embed::factory()))`.
- [ ] **Step 3: Unit-test the factory.**
  - A temporary bridge dir with no file gives `None`.
  - A file with `enabled = false` gives `None`.
  - An invalid file gives `None` and does not panic.
- [ ] **Step 4: Run.** `cargo test -p sapphire-bridge && cargo build -p sapphire-bridge`.
- [ ] **Step 5: Commit.** `feat(bridge): the daemon reads embedding.toml and serves embeddings` (Refs #185).

---

### Task 5: retrieve — trait only, `configure_vectors`, the #194 stop rule

**Files:** `crates/sapphire-framework-retrieve/{Cargo.toml,src/embed.rs,src/config.rs,src/lib.rs,src/retrieve_store.rs,src/redb_store.rs,src/db.rs}`

**Produces:**
- `embed.rs` holds only `pub trait Embedder { fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>; }`.
- `config.rs`: `RetrieveConfig { db: VectorDb, hybrid: HybridConfig }`. `EmbeddingConfig` is removed. Old configs that still contain an `embedding` table must keep loading: `RetrieveConfig` must not use `deny_unknown_fields`, so serde ignores the table. Add a test for it.
- The `RetrieveStore` trait gains:
```rust
/// Make the store hold vectors from `model` with `dim` values. If it already holds vectors
/// from a different model or dimension, they are all dropped (and become pending).
/// Idempotent. Stores without vector support ignore it.
fn configure_vectors(&self, model: &str, dim: u32) -> Result<()> { let _ = (model, dim); Ok(()) }
```
- `RedbStore`:
  - `dim` becomes interior-mutable, for example a `Mutex<Option<u32>>` or an `AtomicU32` where 0 means none.
  - `configure_vectors` compares the meta keys `embedding_model` and `embedding_dim`. On any difference it clears `VECTORS`, writes both keys, and sets `dim`.
  - The existing `open(dir, Some(dim))` path keeps working and writes the dimension only.
- `embed_pending` keeps the #184 behaviour, plus this rule: if a batch fails and every per-item retry of that batch also fails, return that error immediately. Do not attempt the remaining batches.
- Cargo:
  - Remove the `fastembed-embed` feature and the `fastembed`, `ureq` and `tokio` dependencies, if `tokio` is only used by the removed code.
  - `default = ["redb-store"]`.
- `lib.rs`: drop the `build_embedder`, `EmbedderConfig` and `EmbeddingConfig` exports.

- [ ] **Step 1: Failing tests** (`redb_store.rs`):
  - `configure_vectors_on_a_new_model_drops_vectors`: embed with `FakeEmbedder`, call `configure_vectors("other", 3)`, and check `vec_info().vector_count == 0` and `pending_count > 0`.
  - `configure_vectors_with_the_same_model_keeps_vectors`.
  - `configure_vectors_enables_vectors_on_a_store_opened_without_a_dim`: open with `None`, configure with `("m", 3)`, then embed and search.
  - `embed_pending_stops_after_a_fully_failed_batch`: use an always-failing embedder over 150 docs (2 batches) and a call counter. The result is `Err`, and the counter equals 1 batch call plus 100 single calls, with no calls for batch 2.
  - `a_bad_item_in_a_working_batch_does_not_stop_the_rest` (kept from #184).
  - In `config.rs` tests: `an_old_config_with_an_embedding_table_still_loads`.
- [ ] **Step 2: Run them.** `cargo test -p sapphire-framework-retrieve --no-default-features --features redb-store` → fail.
- [ ] **Step 3: Implement** as described above. Delete the REST, fastembed and Ollama code and its tests from `embed.rs`, since that code moved to the bridge crate in Task 3.
- [ ] **Step 4: Run.** `cargo test -p sapphire-framework-retrieve`. This is now the plain default, with no fastembed. Also run `cargo check -p sapphire-framework-retrieve --all-features`. Other crates will fail to compile until Task 6; do not run a workspace check yet.
- [ ] **Step 5: Commit.** `feat(retrieve)!: embedding moves to the bridge — trait only, configure_vectors, stop on a provider outage` (Refs #185, #194, #195).

---

### Task 6: workspace — `BridgeEmbedder`, embedding config removed, fallout

**Files:**
- Create `crates/sapphire-framework-workspace/src/bridge_embedder.rs`.
- Modify `workspace_state.rs`, `config.rs`, `lib.rs` and `Cargo.toml` in the same crate (add the `sapphire-bridge-api` path dependency, drop the `fastembed-embed` feature).
- Fix the fallout in `crates/sapphire-framework/`, `crates/sapphire-framework-backend/` and anything else `cargo check --workspace --all-targets` reports.

**Produces:**
```rust
pub struct BridgeEmbedder { /* tx: std::sync::mpsc::Sender<Job>, info: EmbedModelInfo */ }
impl BridgeEmbedder {
    /// Ask the bridge at `endpoint` (default: the standard bridge endpoint) whether embedding is
    /// enabled. Returns `None` when the bridge is absent or embedding is disabled.
    pub fn connect(endpoint: Option<sapphire_ipc::Endpoint>) -> Option<(BridgeEmbedder, EmbedModelInfo)>;
}
impl sapphire_retrieve::Embedder for BridgeEmbedder { fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>; }
// WorkspaceState
pub fn load_embedder(&self) -> Result<()>;               // idempotent; no config argument
pub async fn load_embedder_async(&self) -> Result<()>;   // runs load_embedder on spawn_blocking
pub fn set_embedder_for_test(&self, e: Box<dyn Embedder + Send + Sync>, model: &str, dim: u32) -> Result<()>; // cfg(any(test, feature="test-util"))
pub async fn sync_and_embed(&self) -> Result<(usize, usize, usize)>;
pub fn embed_pending(&self, on_progress: impl Fn(usize, usize)) -> Result<usize>;
```

`BridgeEmbedder` design:
- `connect` spawns a `std::thread`. The thread builds a `tokio::runtime::Builder::new_current_thread().enable_all()` runtime, connects with `BridgeClient::connect_at` (or `connect`), calls `embed_info`, and sends `Option<EmbedModelInfo>` back over a std channel.
  - If the result is `None` or the connection fails, the thread exits and `connect` returns `None`.
  - Otherwise the thread loops on `rx.recv()`. For each job it calls `client.embed(texts)`. On a connection error it reconnects once and retries, and it replies through the job's std oneshot sender.
- `embed_texts` sends a job and blocks on the reply. It is safe from any thread, because the runtime belongs to its own thread.
- The thread exits when `BridgeEmbedder` drops.

`load_embedder` design:
- If the embedder is already initialized, return.
- Otherwise call `BridgeEmbedder::connect(None)`. On `Some((e, info))`, call `self.retrieve_db().configure_vectors(&info.model, info.dimension)` and store the embedder. On `None`, store `None`.
- Remove `load_retrieve_backend`, `load_retrieve_backend_async`, `extract_vector_config` and `make_vector_backend`. The store opens once, and `configure_vectors` turns vectors on. That removes the double-open suspected in #195.
- `RetrieveConfig.db == VectorDb::None` still means: never load an embedder.
- `sync_and_embed` and `embed_pending` drop their `RetrieveConfig` parameters and call `load_embedder` themselves.

- [ ] **Step 1: Failing tests** (`bridge_embedder.rs` tests). Stand up a fake bridge control endpoint with `sapphire_ipc`:
  - Bind an `Endpoint::in_dir("bridge", tmp)`.
  - Serve a `Router` with handlers for `EMBED_INFO` and `EMBED`, built like the stub bridge in `crates/sapphire-framework-server/src/sync/testing.rs`, from which you can copy the handshake setup.

  Tests:
  - `connect_returns_none_when_no_bridge_listens`.
  - `connect_returns_none_when_embedding_is_disabled`.
  - `embed_texts_round_trips`: 2-dimensional vectors, order kept.
  - `embed_texts_works_from_inside_a_tokio_runtime`: call it inside `#[tokio::test(flavor = "multi_thread")]` without `spawn_blocking`, and assert there is no panic.
  - In `workspace_state.rs` tests: `semantic_search_falls_back_to_fts_without_a_bridge`, and `set_embedder_for_test_enables_semantic_search` (redb, `FakeEmbedder`, dimension 3).
- [ ] **Step 2: Run them.** `cargo test -p sapphire-framework-workspace` → fail.
- [ ] **Step 3: Implement** as described above, then fix the fallout across the workspace:
  - remove references to `EmbeddingConfig`, `build_embedder` and `fastembed-embed`, including the facade's feature list and re-exports;
  - remove `RetrieveConfig.embedding` and callers passing `&RetrieveConfig` to `sync_and_embed` / `embed_pending`.
- [ ] **Step 4: Run.**
  - `cargo check --workspace --all-targets`
  - `cargo test -p sapphire-framework-workspace`
  - `cargo test -p sapphire-framework-backend`
  - `cargo test -p sapphire-framework-server`
  - Known flake: `converge::a_host_that_was_offline_catches_up_when_it_returns`.
- [ ] **Step 5: Commit.** `feat(workspace)!: embed through the bridge; apps no longer configure embedding` (Refs #185, #195).

---

### Task 7: docs, dependency check, whole-workspace tests

- [ ] **Step 1: Check the dependency graph.**
  - `cargo tree -p sapphire-framework --all-features -e normal -i fastembed` must report nothing.
  - So must `cargo tree -p sapphire-framework-bridge -e normal -i candle-core`.
  - `cargo tree -p sapphire-bridge -e normal -i fastembed` must show `sapphire-bridge-embed`.
- [ ] **Step 2: Update the CHANGELOG** (Unreleased → Breaking, plus Added):
  - **Breaking:** embedding moved to the bridge.
  - **Breaking:** `RetrieveConfig.embedding`, `EmbeddingConfig`, `build_embedder` and the `fastembed-embed` feature were removed. Configure embedding in `<bridge dir>/embedding.toml` (fields as in the spec).
  - **Breaking:** journal follow-up needed.
  - **Added:** `sapphire-bridge-embed`, the default model Qwen3-VL-Embedding-2B at 1024 dimensions, and bridge-api 2.2.0.
- [ ] **Step 3: Update `docs/ARCHITECTURE.md`** (Japanese):
  - In the bridge section, the bridge now also embeds.
  - In the retrieve section, the embedder comes from the bridge, and `configure_vectors` is described.
  - The crate table gains `sapphire-bridge-embed` (bridge-only). Note the naming rule: bridge-only components carry the `sapphire-bridge-` prefix, and only facade-reexportable crates carry `sapphire-framework-`.
- [ ] **Step 4: Run the final checks.**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`. This should now run on this host, because fastembed lives only in the bridge-embed crate with dynamic ORT. Report any target that still crashes.
- [ ] **Step 5: Commit.** `docs: embedding in the bridge; naming rule for bridge-only crates` (Closes #185, Closes #194).
