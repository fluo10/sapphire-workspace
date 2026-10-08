# Embedding in the bridge: one model per host, apps embed over IPC

- Date: 2026-10-09
- Issues: #185 (main), #194 (REST fixes folded in); tracking: #189
- Scope:
  - new crate `sapphire-framework-bridge-embed`;
  - `sapphire-framework-bridge` (an `embed` feature, two control-plane methods, `embedding.toml`);
  - `sapphire-framework-bridge-api` (method types, client, own version → 2.2.0);
  - `sapphire-framework-retrieve` (keeps only the `Embedder` trait);
  - `sapphire-framework-workspace` (`BridgeEmbedder`, `RetrieveConfig` without `embedding`);
  - docs.
- Builds on: #184 (one document per file). Branch `feat/bridge-embed` is stacked on
  `feat/remove-chunker` (PR #196).
- Related: #183 (spike: Qwen3-VL-Embedding-2B on CPU), #186 (workgroup-shared settings, API
  key store, CLI/GUI), #187 (synced vector files), #188 (backfill on the designated device).

## Background

Every app builds its own embedder today, from `[retrieve.embedding]` in its own config.
Each app links fastembed, and each one loads its own copy of a local model into RAM.

The #183 spike settled the model. Qwen3-VL-Embedding-2B runs on CPU through fastembed's
`qwen3` feature (candle). It takes about 1 s per short query and 8–12 s per 700-token
document on AVX2 hosts. Its RAM use at f32 is about 7 GB resident, with a peak near 10 GB
while it loads. The spike also found two things to avoid:

- fastembed's default statically linked ONNX Runtime crashes at start-up on CPUs without
  AVX;
- fastembed applies no prompt template and truncates *after* templating.

The fix is to move embedding into the bridge, which is the one per-host daemon:

1. **One model per host.** journal, agent and the others share one loaded model.
2. **Apps lose the heavy dependencies.** fastembed, candle and the REST client leave the app
   graph.
3. **One configuration point.** The bridge owns the embedding settings. #186 then makes them
   workgroup-wide, with per-device API keys.

## Decisions

1. **The bridge embeds; apps ask over IPC.** The control plane gains `embed.info` and
   `embed.embed`.
2. **The local model is Qwen3-VL-Embedding-2B, text mode only.** No other local models for
   now (user decision).
   - It runs at f32. On CPU, f16 is 1.5–2× slower; it saves RAM only.
   - It uses the official template, last-token pooling and L2 normalization.
   - Content is truncated *before* templating, by tokens.
3. **Dimension 1024 by default.** The 2048-dimensional output is cut to the first 1024 values
   (MRL) and then L2-normalized again. This halves storage and the #187 vector files at
   negligible quality cost. It is configurable, and every vector a model produces has the
   same dimension.
4. **REST stays as a provider**, for OpenAI-compatible endpoints. It gets the #194 fixes.
5. **Settings live in `<bridge dir>/embedding.toml` for now.** The file is per device and not
   synced. #186 moves the shared part to the workgroup root and adds key storage, the CLI and
   the GUI. This spec defines only the file and its reading.
6. **No embedding is a normal state.** A missing or disabled configuration, a stopped bridge
   or a model that failed to load all mean the app searches with FTS only. None of them is an
   error.

## `sapphire-framework-bridge-embed` (a bridge component)

This crate is a component of the bridge, and nothing else depends on it. Its name follows the
other bridge crates (`sapphire-framework-bridge`, `sapphire-framework-bridge-api`), under the
project rule that every crate carries the `sapphire-framework-` prefix. Its dependencies:

- fastembed (`default-features = false`, features `qwen3`, `hf-hub-native-tls`,
  `ort-load-dynamic`) behind the feature `local`, which is on by default;
- tokenizers;
- a blocking HTTP client for REST (`ureq`, moved here from retrieve);
- serde and toml;
- tracing.

### Configuration

```toml
# <bridge dir>/embedding.toml
enabled   = true
provider  = "local"            # "local" | "openai"
model     = "Qwen/Qwen3-VL-Embedding-2B"
dimension = 1024               # output dimension after MRL truncation
max_tokens = 1024              # content tokens kept (local only)
# openai only:
endpoint    = "https://api.openai.com"
api_key_env = "OPENAI_API_KEY" # #186 replaces this with a key store
```

```rust
pub struct EmbeddingSettings { pub enabled: bool, pub provider: Provider, pub model: String,
    pub dimension: u32, pub max_tokens: usize, pub endpoint: Option<String>,
    pub api_key_env: Option<String> }
pub enum Provider { Local, OpenAi }
impl EmbeddingSettings { pub fn load(path: &Path) -> Result<Option<Self>> } // None: no file
```

- Defaults: `enabled = false` when the file is absent, `provider = "local"`, the model above,
  `dimension = 1024`, `max_tokens = 1024`.
- `dimension` must be in `64..=2048` for local. An invalid file is an error that the bridge
  logs, after which it treats embedding as disabled.

### Local provider

```rust
pub struct LocalQwen { /* fastembed Qwen3TextEmbedding + tokenizers::Tokenizer */ }
```

- **Template.** The content is wrapped as
  `<|im_start|>system\nRepresent the user's input.<|im_end|>\n<|im_start|>user\n{content}<|im_end|>\n<|im_start|>assistant\n`.
  The constant `TEMPLATE_VERSION = 1` names it, and #187's vector header records it.
- **Truncation.** The content is encoded with the model's tokenizer, without special tokens.
  If it has more than `max_tokens` tokens, it is cut at the byte offset where token
  `max_tokens - 1` ends. Only then is it wrapped. The model's own `max_length` is set to
  `max_tokens + 64`, so the template is never cut.
- **Pooling and dimension.**
  - fastembed returns the last-token, L2-normalized vector of 2048 values. Its normalization
    is done in f32, so there is no f16 issue.
  - Keep the first `dimension` values and L2-normalize again.
- **Weights.** Downloaded by hf-hub into the default Hugging Face cache on first load. A
  failed load is an error for that request. The next request tries again, after a backoff of
  at least 60 s, so a broken network does not cause a download storm.

The pure helpers are unit-tested without the model: `wrap(content)`,
`truncate_at(text, offsets, max)` and `mrl(vec, dim)`.

### REST provider (OpenAI-compatible), with #194

- `MAX_INPUT_CHARS = 4_000` per input. This is a character cap, kept until a tokenizer for
  the remote model is known. Its doc comment states the arithmetic honestly: Japanese
  averages about 1–1.5 cl100k tokens per character, so 4,000 characters stays under the
  8,191-token input limit for typical text. Pathological text can exceed it, and that input
  then fails on its own (see below).
- `MAX_REQUEST_CHARS = 100_000` total per request. Inputs are grouped in order. An input that
  is over the cap on its own goes in a request by itself.
- The response must contain exactly one vector per input. A count mismatch is an error.
- Vectors are cut to `dimension` (MRL) and normalized only when the endpoint returned more
  than `dimension` values. Otherwise they are used as returned. The configured `dimension`
  must match what the endpoint produces, or be smaller.

### The service: one worker, load on demand, unload when idle

```rust
pub trait Embed: Send + Sync { fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
                               fn info(&self) -> ModelInfo; }
pub struct ModelInfo { pub model: String, pub dimension: u32, pub template_version: u32 }

pub struct EmbedService { /* worker thread + request channel */ }
impl EmbedService {
    pub fn start(settings: EmbeddingSettings, idle_unload: Duration) -> EmbedService;
    pub async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;
    pub fn info(&self) -> Option<ModelInfo>;   // None when disabled
}
```

- **One worker thread owns the model.** Requests from any number of control connections
  queue on a channel and are served one at a time, in order. This bounds RAM to one model,
  and CPU inference does not gain from parallel requests.
- **The model loads on the first request**, so a bridge that never embeds never pays for
  it. It **unloads after `IDLE_UNLOAD` (10 min)** with no request, which frees about 7 GB.
- **One request is not split by the service.** A caller that sends 1,000 texts gets them
  embedded in batches of 32 inside the worker, and the results come back together.
- **Errors:**
  - A load failure fails the request with the reason.
  - An inference error fails the request.
  - Neither kills the worker.

## Bridge

- **Cargo feature `embed`**, on by default, pulls in `sapphire-framework-bridge-embed`. Without it
  `embed.info` answers `{ enabled: false }` and `embed.embed` is an error. That keeps a
  slim bridge build possible.
- **Settings.** `embedding.toml` is read at bridge start. A change takes effect on restart;
  #186 adds live reload together with its CLI.
- **Control plane, in `sapphire-framework-bridge-api` 2.2.0.** Everything is additive, and
  `API_VERSION` stays 2.

```rust
pub const EMBED_INFO: &str = "embed.info";
pub const EMBED: &str = "embed.embed";

pub struct EmbedInfoResult { pub enabled: bool, pub model: Option<String>,
                             pub dimension: Option<u32>, pub template_version: Option<u32> }
pub struct EmbedParams { pub texts: Vec<String> }
pub struct EmbedResult { pub model: String, pub dimension: u32, pub vectors: Vec<Vec<f32>> }

impl BridgeClient { pub async fn embed_info(&self) -> Result<EmbedInfoResult>;
                    pub async fn embed(&self, texts: Vec<String>) -> Result<EmbedResult>; }
```

  - **Wire format.** `embed.embed` vectors travel as JSON arrays of f32. That is about 10 KB
    per 1024-dimension vector. The volume is acceptable over local IPC, so no binary framing
    is added.
  - **Version number.** The number 2.2.0 avoids colliding with #193, which takes 2.1.0. If
    #193 merges first, this branch is rebased and still lands as 2.2.0.
- **Status.** `bridge status` and `status.json` gain an `embedding` line. It shows disabled,
  or model, dimension, and whether the model is loaded.

## Retrieve

`sapphire-framework-retrieve` keeps the `Embedder` trait, which is the interface the store
calls:

```rust
pub trait Embedder: Send + Sync { fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>; }
```

Removed from retrieve:

- `build_embedder`, `EmbedderConfig` and the OpenAI, Ollama and fastembed implementations;
- the `fastembed-embed` feature, and the `ureq` and `fastembed` dependencies;
- `config::EmbeddingConfig`.

`RetrieveConfig` keeps `db` and `hybrid`. Since retrieve no longer links a statically built
ONNX Runtime, the workspace-wide `cargo test` runs on CPUs without AVX again.

`embed_pending` keeps its batch, per-item retry and pending behaviour from #184. It adds the
#194 rule: if every single retry in a batch fails, the remaining batches are not attempted.
They stay pending, and the call returns the error. This stops a provider outage from
turning into N requests. The per-item retry still isolates one bad input inside a batch
that otherwise works.

## Workspace

- **`BridgeEmbedder`** implements `retrieve::Embedder`.
  - On construction it starts its own thread, which runs a current-thread tokio runtime
    holding a `BridgeClient`.
  - `embed_texts` sends the request over a channel and blocks on the reply.
  - It is therefore safe to call from any thread, including from inside another runtime,
    which `Handle::block_on` would not be.
  - It reconnects on the next call if the bridge went away.
- **Workspace dependency.** The workspace gains a dependency on
  `sapphire-framework-bridge-api`, which is serde plus `sapphire-ipc`; no iroh is pulled in.
- **`WorkspaceState::load_embedder()`** takes no config argument. It asks `embed.info` once:
  - When the answer is enabled, it installs a `BridgeEmbedder` and configures the vector store
    with the reported `dimension`, which replaces `extract_vector_config`'s dimension from the
    app config.
  - When the answer is disabled, or the bridge is unreachable, there is no embedder, and
    search falls back to FTS as it does today.
- **`RetrieveConfig.embedding` is removed**, and so is `RetrieveParams`'s embedding part. Apps
  no longer configure embedding.
- **A model change.** If `embed.info`'s model or dimension differs from what the store was
  built with (stored in the store's meta as `embedding_model`), the store's vectors are
  dropped. They are then re-embedded as pending.

## Downstream

sapphire-journal configures embedding in its own config (`[cache.retrieve.embedding]`). It
calls `build_embedder` and enables `fastembed-embed`. All of that goes away, and a
follow-up issue is filed there. Timer and agent do not configure embedding.

## Error handling

- **Bridge unreachable or embedding disabled.** There is no embedder. Search is FTS only,
  and `embed_pending` is not called.
- **Bridge reachable but `embed.embed` fails** (model load error, REST error). The calling
  documents stay pending, and the error is logged by the app with the bridge's message.
- **Bridge stops mid-session.** `BridgeEmbedder` returns an error for that call and
  reconnects on the next one.
- **Invalid `embedding.toml`.** The bridge logs the error at start and in `status`, and
  embedding is disabled.

## Testing

- **embed crate, without the model:**
  - `wrap`, `truncate_at` (ASCII, Japanese, exactly at the limit, under the limit);
  - `mrl` (cuts to `dim` and re-normalizes to unit length);
  - settings parsing (defaults, invalid dimension);
  - REST grouping (order kept, over-cap input sent alone, empty input means no request) and
    the count-mismatch error, tested with a fake HTTP layer behind a small trait;
  - the service with a fake `Embed`:
    - requests are served in order;
    - load happens on the first request;
    - unload happens after a short idle (the duration is injectable);
    - a load failure fails the request without killing the worker.
- **embed crate, with the model (`#[ignore]`):** one real embedding. It checks a dimension
  of 1024, a unit norm, and that similar Japanese sentences are closer than unrelated ones.
- **bridge:** `embed.info` and `embed.embed` over the control plane with a fake service
  injected, plus `embed.info` when disabled.
- **workspace:** `BridgeEmbedder` round-trip against a bridge test fixture with a fake
  service, and fallback to FTS when the bridge is absent.
- **retrieve:** the #194 stop-after-a-fully-failed-batch rule.
- **Whole workspace:** `cargo test --workspace` now runs on this host, because fastembed is
  out of every crate but the embed crate. The bridge's own tests run with the fake service.

## Out of scope

- Workgroup-shared settings, the API key store, and the CLI and GUI for settings: #186.
- Synced vector files and the template and model header: #187.
- Backfill on the designated device: #188.
- Image embedding: text only for now.
- GPU backends.
