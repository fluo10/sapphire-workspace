use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use sapphire_workspace::{UserConfig, Workspace, WorkspaceState, dedup_chunk_results};

use crate::WORKSPACE_CTX;
use serde::Deserialize;

// ── server struct ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct RecallServer {
    default_dir: Option<PathBuf>,
    state: Arc<Mutex<Option<WorkspaceState>>>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for RecallServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecallServer")
            .field("default_dir", &self.default_dir)
            .finish_non_exhaustive()
    }
}

impl RecallServer {
    fn new(workspace_dir: Option<PathBuf>) -> Self {
        Self {
            default_dir: workspace_dir,
            state: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    fn with_state<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&WorkspaceState) -> anyhow::Result<T>,
    {
        let mut guard = self.state.lock().unwrap();
        if guard.is_none() {
            let workspace = Workspace::resolve(&WORKSPACE_CTX, self.default_dir.as_deref())?;
            let state = WorkspaceState::open(workspace)?;
            let config = UserConfig::load()?;
            if config
                .retrieve
                .as_ref()
                .and_then(|r| r.embedding.as_ref())
                .map(|e| e.enabled)
                .unwrap_or(false)
            {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        state.load_retrieve_backend_async(&config).await?;
                        state.load_embedder_async(&config).await
                    })
                })?;
            }
            *guard = Some(state);
        }
        f(guard.as_ref().unwrap())
    }
}

// ── parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// Search query text. Supports substring and CJK queries (FTS5 trigram index).
    query: String,
    /// Maximum number of results to return (default: 10).
    limit: Option<usize>,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router]
impl RecallServer {
    #[tool(
        description = "Show workspace location, index path, schema version, and document count."
    )]
    fn workspace_info(&self, _: Parameters<serde_json::Value>) -> Result<String, String> {
        (|| -> anyhow::Result<String> {
            self.with_state(|s| {
                let info = s.db_info()?;
                Ok(format!(
                    "workspace:      {}\ndb path:        {}\nschema version: v{}\ndocuments:      {}",
                    s.workspace.root.display(),
                    info.db_path.display(),
                    info.schema_version,
                    info.document_count,
                ))
            })
        })()
        .map_err(|e| e.to_string())
    }

    #[tool(description = "Incrementally sync the workspace index. \
        Walks the workspace directory, upserts new/changed documents, and removes \
        documents for deleted files. Returns the number of files synced.")]
    fn workspace_sync(&self, _: Parameters<serde_json::Value>) -> Result<String, String> {
        (|| -> anyhow::Result<String> {
            self.with_state(|s| {
                let (upserted, _removed) = s.sync()?;
                Ok(format!("synced: {upserted} files"))
            })
        })()
        .map_err(|e| e.to_string())
    }

    #[tool(description = "Rebuild the workspace index from scratch. \
        Deletes the current index and re-indexes all files. \
        Returns the number of files indexed.")]
    fn workspace_rebuild(&self, _: Parameters<serde_json::Value>) -> Result<String, String> {
        (|| -> anyhow::Result<String> {
            let mut guard = self.state.lock().unwrap();
            let workspace_root = match guard.as_ref() {
                Some(s) => s.workspace.root.clone(),
                None => Workspace::resolve(&WORKSPACE_CTX, self.default_dir.as_deref())?.root,
            };
            let state =
                WorkspaceState::rebuild(Workspace::from_root(&WORKSPACE_CTX, &workspace_root)?)?;
            let (upserted, _removed) = state.sync()?;
            *guard = Some(state);
            Ok(format!("rebuilt: {upserted} files indexed"))
        })()
        .map_err(|e| e.to_string())
    }

    #[tool(description = "Search indexed documents. \
        When `embedding.enabled = true` in the user config, uses approximate \
        (vector/semantic) search. Otherwise falls back to full-text search \
        (FTS5 trigram index, supports substring and CJK queries). \
        Returns a JSON array of results ordered by relevance, each with \
        `id`, `title`, `path`, and `score`.")]
    fn search(&self, Parameters(p): Parameters<SearchParams>) -> Result<String, String> {
        (|| -> anyhow::Result<String> {
            self.with_state(|s| {
                s.sync()?;
                let limit = p.limit.unwrap_or(10);

                if let Some(embedder) = s.embedder() {
                    let pending = s
                        .retrieve_db()
                        .vec_info()
                        .map(|vi| vi.pending_count)
                        .unwrap_or(0);
                    if pending > 0 && pending <= 50 {
                        let _ = s.retrieve_db().embed_pending(embedder, &|_, _| {});
                    }

                    let query_vecs = embedder.embed_texts(&[p.query.as_str()])?;
                    let query_vec = query_vecs
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("embedder returned empty result"))?;
                    let raw = s
                        .retrieve_db()
                        .search_similar(&query_vec, limit * 3)
                        .map_err(anyhow::Error::msg)?;
                    let results = dedup_chunk_results(raw, limit);
                    return Ok(serde_json::to_string_pretty(&results)?);
                }

                let results = s
                    .retrieve_db()
                    .search_fts(&p.query, limit)
                    .map_err(anyhow::Error::msg)?;
                Ok(serde_json::to_string_pretty(&results)?)
            })
        })()
        .map_err(|e| e.to_string())
    }
}

#[tool_handler]
impl ServerHandler for RecallServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "sapphire-workspace indexes text files for full-text and semantic search. \
                 Use workspace_sync to keep the index up to date, workspace_info to \
                 inspect the index, workspace_rebuild to recreate it from scratch, \
                 and search to find relevant documents."
                .to_owned(),
        )
    }
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
pub async fn run(workspace_dir: Option<&Path>) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let server = RecallServer::new(workspace_dir.map(|p| p.to_path_buf()));
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
