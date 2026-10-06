//! A [`WorkspaceBackend`] that forwards every call to the application's server.
//!
//! The server is the only process that may open the cache, so a CLI, a stdio MCP server or
//! a desktop UI holds one of these instead of a [`LocalBackend`](crate::LocalBackend). The
//! two are interchangeable: that is the whole point of the trait.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sapphire_ipc::{Client, ClientInfo, Endpoint, connect_or_absent};
use tokio::sync::broadcast;

use crate::protocol as proto;
use crate::{BackendEvent, FileSearchResult, Result, SearchMode, SyncSummary, WorkspaceBackend};

/// Capacity of the local event fan-out. Matches `LocalBackend`'s, so a subscriber behaves
/// the same whichever backend it holds.
const EVENT_CAPACITY: usize = 128;

/// A [`WorkspaceBackend`] over an IPC connection to the application's server.
#[derive(Debug)]
pub struct IpcBackend {
    client: Arc<Client>,
    ws: PathBuf,
    events: broadcast::Sender<BackendEvent>,
}

impl IpcBackend {
    /// Connect to `app`'s server, and bind to one workspace.
    ///
    /// Nothing is started here: a server runs under `serve` or the OS service manager,
    /// and this only finds it. Nothing listening is an error — the caller decides whether
    /// to start one.
    pub async fn connect(
        endpoint: &Endpoint,
        app: &str,
        kind: &str,
        version: &str,
        ws: PathBuf,
    ) -> Result<IpcBackend> {
        let info = ClientInfo {
            kind: kind.to_owned(),
            version: version.to_owned(),
            api: proto::API_VERSION,
            pid: std::process::id(),
        };
        let (client, _) = connect_or_absent(endpoint, app, info)
            .await?
            .ok_or_else(|| {
                sapphire_ipc::Error::NotRunning(format!(
                    "no {app} server is running; start it with `{app} serve` \
                     or install its service"
                ))
            })?;
        Ok(IpcBackend::from_client(Arc::new(client), ws))
    }

    /// Bind an existing client to one workspace.
    ///
    /// An application that already has a connection — because it also calls its own
    /// methods — passes it here rather than opening a second one.
    pub fn from_client(client: Arc<Client>, ws: PathBuf) -> IpcBackend {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let backend = IpcBackend {
            client: Arc::clone(&client),
            ws: ws.clone(),
            events: events.clone(),
        };

        // Translate this workspace's notifications into BackendEvents.
        let mut notifications = client.notifications();
        tokio::spawn(async move {
            loop {
                match notifications.recv().await {
                    Ok(n) if n.method == proto::EVENT => {
                        let Ok(params) = serde_json::from_value::<proto::EventParams>(n.params)
                        else {
                            tracing::warn!("dropping malformed event notification");
                            continue;
                        };
                        if params.ws == ws {
                            let _ = events.send(params.event);
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        backend
    }

    /// The underlying client, for an application's own methods.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// Ask the server to start sending this workspace's events.
    ///
    /// Called by [`subscribe`](WorkspaceBackend::subscribe) is not possible — that method is
    /// synchronous — so a caller that wants events calls this once after connecting.
    pub async fn start_events(&self) -> Result<()> {
        let _: proto::Ack = self
            .client
            .call(
                proto::SUBSCRIBE,
                proto::WsParams {
                    ws: self.ws.clone(),
                },
            )
            .await?;
        Ok(())
    }
}

#[async_trait]
impl WorkspaceBackend for IpcBackend {
    async fn search(
        &self,
        query: &str,
        limit: usize,
        mode: SearchMode,
    ) -> Result<Vec<FileSearchResult>> {
        let result: proto::SearchResult = self
            .client
            .call(
                proto::SEARCH,
                proto::SearchParams {
                    ws: self.ws.clone(),
                    query: query.to_owned(),
                    limit,
                    mode,
                },
            )
            .await?;
        Ok(result.hits)
    }

    async fn read_file(&self, path: &Path) -> Result<String> {
        let result: proto::ReadResult = self
            .client
            .call(
                proto::READ_FILE,
                proto::PathParams {
                    ws: self.ws.clone(),
                    path: path.to_owned(),
                },
            )
            .await?;
        Ok(result.content)
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        let _: proto::Ack = self
            .client
            .call(
                proto::WRITE_FILE,
                proto::ContentParams {
                    ws: self.ws.clone(),
                    path: path.to_owned(),
                    content: content.to_owned(),
                },
            )
            .await?;
        Ok(())
    }

    async fn append_file(&self, path: &Path, content: &str) -> Result<()> {
        let _: proto::Ack = self
            .client
            .call(
                proto::APPEND_FILE,
                proto::ContentParams {
                    ws: self.ws.clone(),
                    path: path.to_owned(),
                    content: content.to_owned(),
                },
            )
            .await?;
        Ok(())
    }

    async fn delete_file(&self, path: &Path) -> Result<()> {
        let _: proto::Ack = self
            .client
            .call(
                proto::DELETE_FILE,
                proto::PathParams {
                    ws: self.ws.clone(),
                    path: path.to_owned(),
                },
            )
            .await?;
        Ok(())
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<(PathBuf, bool)>> {
        let result: proto::ListDirResult = self
            .client
            .call(
                proto::LIST_DIR,
                proto::PathParams {
                    ws: self.ws.clone(),
                    path: path.to_owned(),
                },
            )
            .await?;
        Ok(result
            .entries
            .into_iter()
            .map(|e| (e.path, e.is_dir))
            .collect())
    }

    async fn sync(&self) -> Result<SyncSummary> {
        let result: proto::ReindexResult = self
            .client
            .call(
                proto::REINDEX,
                proto::WsParams {
                    ws: self.ws.clone(),
                },
            )
            .await?;
        Ok(SyncSummary {
            upserted: result.upserted,
            removed: result.removed,
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }
}
