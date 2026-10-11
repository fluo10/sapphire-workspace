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
    events: broadcast::Sender<BackendEvent>,
}

impl IpcBackend {
    /// Connect to `app`'s server. It serves one workspace; this backend acts on whichever
    /// is current.
    ///
    /// `app_api` is the version of the application's own API the caller expects, when it
    /// will call the application's methods over [`client`](Self::client) as well; the
    /// handshake refuses a server that speaks another. `None` asks for the framework's
    /// methods only.
    ///
    /// Nothing is started here: a server runs under `serve` or the OS service manager,
    /// and this only finds it. Nothing listening is an error — the caller decides whether
    /// to start one.
    pub async fn connect(
        endpoint: &Endpoint,
        app: &str,
        kind: &str,
        version: &str,
        app_api: Option<u32>,
    ) -> Result<IpcBackend> {
        let info = ClientInfo {
            kind: kind.to_owned(),
            version: version.to_owned(),
            api: proto::API_VERSION,
            app_api,
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
        Ok(IpcBackend::from_client(Arc::new(client)))
    }

    /// A backend over an existing client.
    ///
    /// An application that already has a connection — because it also calls its own
    /// methods — passes it here rather than opening a second one.
    pub fn from_client(client: Arc<Client>) -> IpcBackend {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let backend = IpcBackend {
            client: Arc::clone(&client),
            events: events.clone(),
        };

        // Translate the server's notifications into BackendEvents.
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
                        let _ = events.send(params.event);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        backend
    }

    /// The workspace the server serves now, if it has one.
    pub async fn current(&self) -> Result<Option<proto::CurrentWorkspace>> {
        let result: proto::WorkspaceCurrentResult = self
            .client
            .call(proto::WORKSPACE_CURRENT, serde_json::json!({}))
            .await?;
        Ok(result.workspace)
    }

    /// Refuse to act from inside another workspace of `app` than the server's (#215).
    ///
    /// A CLI never picks a workspace by its current directory any more: the server serves
    /// one. But a user standing in another workspace's directory expects that one, so a
    /// command run from there fails and says how to switch, rather than silently acting
    /// on the server's. Outside any workspace, anything goes. Also fails when the server
    /// has no workspace yet.
    pub async fn check_cwd(&self, app: &str) -> Result<std::path::PathBuf> {
        let current = self.current().await?.ok_or(crate::Error::NoWorkspace)?;
        let cwd = std::env::current_dir().map_err(sapphire_ipc::Error::Io)?;
        match cwd_conflict(app, &current.root, &cwd) {
            Some(cwd_workspace) => Err(crate::Error::OtherWorkspace {
                cwd_workspace,
                current: current.root,
            }),
            None => Ok(current.root),
        }
    }

    /// The underlying client, for an application's own methods.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// Ask the server to start sending the current workspace's events.
    ///
    /// Called by [`subscribe`](WorkspaceBackend::subscribe) is not possible — that method is
    /// synchronous — so a caller that wants events calls this once after connecting.
    pub async fn start_events(&self) -> Result<()> {
        let _: proto::Ack = self
            .client
            .call(proto::SUBSCRIBE, serde_json::json!({}))
            .await?;
        Ok(())
    }
}

/// The workspace of `app` that `cwd` is inside, when it is not `current`.
///
/// Walks up from `cwd` to the nearest `.<app>` marker; the directory holding it is the
/// workspace `cwd` is in. `None` when there is none, or when it is `current`.
pub fn cwd_conflict(app: &str, current: &Path, cwd: &Path) -> Option<PathBuf> {
    let marker = format!(".{app}");
    let found = cwd.ancestors().find(|d| d.join(&marker).is_dir())?;
    let found = found.canonicalize().unwrap_or_else(|_| found.to_path_buf());
    let current = current
        .canonicalize()
        .unwrap_or_else(|_| current.to_path_buf());
    (found != current).then_some(found)
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
            .call(proto::REINDEX, serde_json::json!({}))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cwd_conflicts_only_inside_another_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for root in [&a, &b] {
            std::fs::create_dir_all(root.join(".app").join("x")).unwrap();
        }
        std::fs::create_dir_all(a.join("notes/deep")).unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        assert_eq!(cwd_conflict("app", &a, &a.join("notes/deep")), None);
        assert_eq!(
            cwd_conflict("app", &a, &plain),
            None,
            "outside any workspace"
        );
        assert_eq!(cwd_conflict("app", &a, &b), Some(b.canonicalize().unwrap()));
        assert_eq!(cwd_conflict("other", &a, &b), None, "another app's marker");
    }
}
