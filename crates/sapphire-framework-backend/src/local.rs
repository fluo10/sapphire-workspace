//! Local backend: an async facade over the synchronous
//! [`WorkspaceState`](sapphire_workspace::WorkspaceState).
//!
//! Every blocking workspace call runs on the tokio blocking pool via
//! `spawn_blocking`, keeping the async runtime responsive. Mutating operations
//! emit a [`BackendEvent`](crate::BackendEvent) after they succeed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sapphire_workspace::{HybridConfig, RetrieveParams, WorkspaceState};
use tokio::sync::broadcast;

use crate::{BackendEvent, FileSearchResult, Result, SearchMode, SyncSummary, WorkspaceBackend};

/// Default capacity of the event broadcast channel. Slow subscribers that fall
/// this far behind observe a lagged receiver rather than blocking producers.
const EVENT_CAPACITY: usize = 128;

/// Run a search against a [`WorkspaceState`] (blocking). Shared by
/// [`LocalBackend`] and the remote backend's local cache so both search the
/// same way.
pub(crate) fn search_state(
    state: &WorkspaceState,
    query: &str,
    limit: usize,
    mode: SearchMode,
) -> sapphire_workspace::Result<Vec<FileSearchResult>> {
    let mode = match mode {
        SearchMode::Fts => sapphire_workspace::SearchMode::Fts,
        SearchMode::Semantic => sapphire_workspace::SearchMode::Semantic,
        SearchMode::Hybrid => sapphire_workspace::SearchMode::Hybrid,
    };
    let params = RetrieveParams {
        query,
        limit,
        mode,
        folder: None,
    };
    let hits = state.retrieve_files(&params, &HybridConfig::default())?;
    Ok(hits
        .into_iter()
        .map(|hit| FileSearchResult {
            id: hit.id,
            path: hit.path,
            score: hit.score,
            snippet: hit.snippet,
        })
        .collect())
}

/// A [`WorkspaceBackend`] backed by a local [`WorkspaceState`].
pub struct LocalBackend {
    state: Arc<WorkspaceState>,
    events: broadcast::Sender<BackendEvent>,
}

impl LocalBackend {
    /// Wrap an already-open [`WorkspaceState`].
    pub fn new(state: Arc<WorkspaceState>) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self { state, events }
    }

    /// Borrow the underlying workspace state (e.g. to configure embedders).
    pub fn state(&self) -> &Arc<WorkspaceState> {
        &self.state
    }

    /// Emit an event, ignoring the "no subscribers" case.
    fn emit(&self, event: BackendEvent) {
        let _ = self.events.send(event);
    }
}

#[async_trait]
impl WorkspaceBackend for LocalBackend {
    async fn search(
        &self,
        query: &str,
        limit: usize,
        mode: SearchMode,
    ) -> Result<Vec<FileSearchResult>> {
        let state = Arc::clone(&self.state);
        let query = query.to_owned();
        let hits = tokio::task::spawn_blocking(move || search_state(&state, &query, limit, mode))
            .await??;
        Ok(hits)
    }

    async fn read_file(&self, path: &Path) -> Result<String> {
        let state = Arc::clone(&self.state);
        let path = path.to_owned();
        Ok(tokio::task::spawn_blocking(move || state.read_file(&path)).await??)
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        let state = Arc::clone(&self.state);
        let owned = path.to_owned();
        let content = content.to_owned();
        let for_task = owned.clone();
        tokio::task::spawn_blocking(move || state.write_file(&for_task, &content)).await??;
        self.emit(BackendEvent::FileChanged { path: owned });
        Ok(())
    }

    async fn append_file(&self, path: &Path, content: &str) -> Result<()> {
        let state = Arc::clone(&self.state);
        let owned = path.to_owned();
        let content = content.to_owned();
        let for_task = owned.clone();
        tokio::task::spawn_blocking(move || state.append_file(&for_task, &content)).await??;
        self.emit(BackendEvent::FileChanged { path: owned });
        Ok(())
    }

    async fn delete_file(&self, path: &Path) -> Result<()> {
        let state = Arc::clone(&self.state);
        let owned = path.to_owned();
        let for_task = owned.clone();
        tokio::task::spawn_blocking(move || state.delete_file(&for_task)).await??;
        self.emit(BackendEvent::FileRemoved { path: owned });
        Ok(())
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<(PathBuf, bool)>> {
        let state = Arc::clone(&self.state);
        let path = path.to_owned();
        Ok(tokio::task::spawn_blocking(move || state.list_dir(&path)).await??)
    }

    async fn sync(&self) -> Result<SyncSummary> {
        let state = Arc::clone(&self.state);
        let (upserted, removed) = tokio::task::spawn_blocking(move || state.sync()).await??;
        self.emit(BackendEvent::Synced { upserted, removed });
        Ok(SyncSummary { upserted, removed })
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_workspace::{AppContext, Workspace};

    fn ctx() -> &'static AppContext {
        static CTX: std::sync::OnceLock<AppContext> = std::sync::OnceLock::new();
        CTX.get_or_init(|| AppContext::new("backend-local-test"))
    }

    fn backend() -> (tempfile::TempDir, LocalBackend) {
        let tmp = tempfile::Builder::new().prefix("be-").tempdir().unwrap();
        ctx().set_cache_dir(std::env::temp_dir().join("backend-local-cache"));
        std::fs::create_dir_all(tmp.path().join(".backend-local-test")).unwrap();
        let ws = Workspace::from_root(ctx(), tmp.path()).unwrap();
        let state = WorkspaceState::open(ws).unwrap();
        (tmp, LocalBackend::new(Arc::new(state)))
    }

    #[tokio::test]
    async fn write_then_search_and_event() {
        let (_tmp, backend) = backend();
        let mut rx = backend.subscribe();

        backend
            .write_file(Path::new("note.md"), "the quick brown fox")
            .await
            .unwrap();

        // A FileChanged event was published.
        let ev = rx.try_recv().unwrap();
        assert!(matches!(ev, BackendEvent::FileChanged { .. }));

        let hits = backend.search("quick", 10, SearchMode::Fts).await.unwrap();
        assert!(
            hits.iter().any(|h| h.path.ends_with("note.md")),
            "expected note.md in {hits:?}"
        );
    }

    #[tokio::test]
    async fn sync_emits_summary() {
        let (tmp, backend) = backend();
        std::fs::write(tmp.path().join("a.md"), "alpha").unwrap();
        let summary = backend.sync().await.unwrap();
        assert_eq!(summary.upserted, 1);
        assert_eq!(summary.removed, 0);
    }
}
