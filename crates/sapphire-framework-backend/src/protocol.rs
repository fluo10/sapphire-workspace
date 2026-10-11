//! The `workspace.*` namespace: method names and their parameter and result types.
//!
//! Both sides of the connection name these types — the server in
//! `sapphire-framework-server`, the client in [`IpcBackend`](crate::IpcBackend) — so they
//! live here, beside the [`WorkspaceBackend`](crate::WorkspaceBackend) trait they mirror,
//! rather than in `sapphire-framework-ipc`, which must stay free of the search stack.
//!
//! A server serves one workspace at a time (#215), so no request names one: every
//! `workspace.*` and `sync.*` method acts on the server's current workspace, which
//! [`WORKSPACE_SELECT`] and [`WORKSPACE_INIT`] change and [`WORKSPACE_CURRENT`] reports.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{BackendEvent, FileSearchResult, SearchMode};

/// The version of the app server's API: the framework's methods below and their types.
///
/// This, not the crate version, is what a CLI, MCP server or desktop UI must agree on
/// with the running server, so that a release that leaves the method set alone does not
/// demand a service restart. Bump it on a breaking change to a method, a parameter or a
/// result here.
///
/// 2: `workspace.list`, `workspace.forget`.
///
/// 3: search results carry `snippet` in place of `chunks` (#184).
///
/// 4: one workspace per server (#215): no `ws` parameter, `workspace.select` and
/// `workspace.current` in place of `workspace.list` and `workspace.forget`.
pub const API_VERSION: u32 = 4;

/// Search the workspace.
pub const SEARCH: &str = "workspace.search";
/// Read a text file in full.
pub const READ_FILE: &str = "workspace.read_file";
/// Create or overwrite a text file.
pub const WRITE_FILE: &str = "workspace.write_file";
/// Append to a text file.
pub const APPEND_FILE: &str = "workspace.append_file";
/// Delete a file.
pub const DELETE_FILE: &str = "workspace.delete_file";
/// List a directory's direct children.
pub const LIST_DIR: &str = "workspace.list_dir";
/// Rebuild the index from disk.
///
/// Named `reindex`, not `sync`: [`WorkspaceBackend::sync`](crate::WorkspaceBackend::sync)
/// means "walk the files and update the index", which reads as peer-to-peer sync once the
/// bridge exists.
pub const REINDEX: &str = "workspace.reindex";
/// Start receiving [`EVENT`] notifications for the current workspace, and for whichever
/// workspace is current after a switch.
pub const SUBSCRIBE: &str = "workspace.subscribe";
/// Create this app's workspace home in a directory — the marker and a registry entry —
/// and make it the current workspace.
pub const WORKSPACE_INIT: &str = "workspace.init";
/// Make an existing workspace of this app the current one.
pub const WORKSPACE_SELECT: &str = "workspace.select";
/// Report the current workspace.
pub const WORKSPACE_CURRENT: &str = "workspace.current";

/// Parameters of [`WORKSPACE_SELECT`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceSelectParams {
    /// The workspace's root: absolute, or relative to the server's cwd.
    pub dir: PathBuf,
}

/// Result of [`WORKSPACE_CURRENT`].
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WorkspaceCurrentResult {
    /// The current workspace; `None` before one is selected.
    pub workspace: Option<CurrentWorkspace>,
}

/// The workspace a server serves, as [`WORKSPACE_CURRENT`] and [`WORKSPACE_SELECT`]
/// report it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CurrentWorkspace {
    /// Its root on this host.
    pub root: PathBuf,
    /// Its identity across devices, when one has been minted. Read, never minted.
    pub workspace_id: Option<grain_id::GrainId>,
    /// Whether the root still holds this application's marker directory.
    pub reachable: bool,
    /// Its replication state, as [`SYNC_STATUS`] reports it.
    pub sync: SyncStatusResult,
}

/// Parameters of [`WORKSPACE_INIT`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceInitParams {
    /// Where the workspace root goes, relative to nothing: absolute, or relative
    /// to the process's cwd, resolved by the server.
    pub dir: PathBuf,
}

/// Result of [`WORKSPACE_INIT`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceInitResult {
    /// The workspace's canonical root.
    pub root: PathBuf,
    /// Its stable id, as the registry keys it.
    pub workspace_id: String,
    /// `false` when the workspace already existed (exit 0 either way).
    pub created: bool,
}
/// Notification carrying one [`BackendEvent`].
pub const EVENT: &str = "workspace.event";
/// What the server knows about itself.
pub const SERVER_INFO: &str = "server.info";

/// Parameters naming a path inside the workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathParams {
    /// Workspace-relative path.
    pub path: PathBuf,
}

/// Parameters naming a path and the text to put there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentParams {
    /// Workspace-relative path.
    pub path: PathBuf,
    /// The text.
    pub content: String,
}

/// Parameters of [`SEARCH`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchParams {
    /// The query.
    pub query: String,
    /// Maximum number of files to return.
    pub limit: usize,
    /// Which retrieval strategy to use.
    #[serde(default)]
    pub mode: SearchMode,
}

/// Result of [`SEARCH`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    /// File-level hits, best first.
    pub hits: Vec<FileSearchResult>,
}

/// Result of [`READ_FILE`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadResult {
    /// The file's contents.
    pub content: String,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirEntry {
    /// The child's path.
    pub path: PathBuf,
    /// Whether it is a directory.
    pub is_dir: bool,
}

/// Result of [`LIST_DIR`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListDirResult {
    /// The direct children.
    pub entries: Vec<DirEntry>,
}

/// Result of [`REINDEX`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ReindexResult {
    /// Documents added or updated.
    pub upserted: usize,
    /// Documents removed.
    pub removed: usize,
}

/// The success payload of a method that returns nothing.
///
/// An empty object rather than `null`, so that a later release can add a field without
/// changing the shape of the response.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Ack {}

/// Parameters of an [`EVENT`] notification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventParams {
    /// The root of the workspace the event came from: the current one when it was sent.
    pub ws: PathBuf,
    /// What happened.
    pub event: BackendEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::from_value(serde_json::to_value(value).unwrap()).unwrap()
    }

    #[test]
    fn search_parameters_round_trip() {
        let params = SearchParams {
            query: "hello".into(),
            limit: 10,
            mode: SearchMode::Fts,
        };
        let back = round_trip(&params);
        assert_eq!(back.query, "hello");
        assert_eq!(back.limit, 10);
        assert_eq!(back.mode, SearchMode::Fts);
    }

    #[test]
    fn every_search_mode_has_a_stable_lowercase_name() {
        for (mode, name) in [
            (SearchMode::Fts, "fts"),
            (SearchMode::Semantic, "semantic"),
            (SearchMode::Hybrid, "hybrid"),
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), serde_json::json!(name));
            assert_eq!(
                serde_json::from_value::<SearchMode>(serde_json::json!(name)).unwrap(),
                mode
            );
        }
    }

    #[test]
    fn a_missing_mode_defaults_to_hybrid() {
        let value = serde_json::json!({ "query": "q", "limit": 5 });
        let params: SearchParams = serde_json::from_value(value).unwrap();
        assert_eq!(params.mode, SearchMode::Hybrid);
    }

    #[test]
    fn every_backend_event_round_trips() {
        for event in [
            BackendEvent::Synced {
                upserted: 3,
                removed: 1,
            },
            BackendEvent::FileChanged {
                path: PathBuf::from("a.md"),
            },
            BackendEvent::FileRemoved {
                path: PathBuf::from("b.md"),
            },
            BackendEvent::Error {
                message: "boom".into(),
            },
        ] {
            assert_eq!(round_trip(&event), event);
        }
    }

    #[test]
    fn a_directory_listing_round_trips() {
        let listing = ListDirResult {
            entries: vec![
                DirEntry {
                    path: PathBuf::from("notes"),
                    is_dir: true,
                },
                DirEntry {
                    path: PathBuf::from("a.md"),
                    is_dir: false,
                },
            ],
        };
        let back = round_trip(&listing);
        assert_eq!(back.entries.len(), 2);
        assert!(back.entries[0].is_dir);
        assert!(!back.entries[1].is_dir);
    }

    #[test]
    fn an_ack_is_an_object_not_null() {
        assert_eq!(serde_json::to_value(Ack {}).unwrap(), serde_json::json!({}));
    }

    #[test]
    fn workspace_init_params_and_result_round_trip() {
        let params = WorkspaceInitParams {
            dir: PathBuf::from("/home/me/papers"),
        };
        let back = round_trip(&params);
        assert_eq!(back.dir, params.dir);

        let result = WorkspaceInitResult {
            root: PathBuf::from("/home/me/papers"),
            workspace_id: "g123".into(),
            created: true,
        };
        let back = round_trip(&result);
        assert_eq!(back.root, result.root);
        assert_eq!(back.workspace_id, result.workspace_id);
        assert!(back.created);
    }

    #[test]
    fn the_current_workspace_round_trips() {
        let current = CurrentWorkspace {
            root: PathBuf::from("/home/me/notes"),
            workspace_id: Some(grain_id::GrainId::random()),
            reachable: true,
            sync: SyncStatusResult::not_synced(),
        };
        let back = round_trip(&WorkspaceCurrentResult {
            workspace: Some(current.clone()),
        });
        let back = back.workspace.unwrap();
        assert_eq!(back.root, current.root);
        assert_eq!(back.workspace_id, current.workspace_id);
        assert!(
            round_trip(&WorkspaceCurrentResult::default())
                .workspace
                .is_none()
        );
        assert_eq!(
            round_trip(&WorkspaceSelectParams {
                dir: PathBuf::from("/x")
            })
            .dir,
            PathBuf::from("/x")
        );
        assert_eq!(API_VERSION, 4);
    }

    #[test]
    fn method_names_are_namespaced() {
        for name in [
            SEARCH,
            READ_FILE,
            WRITE_FILE,
            APPEND_FILE,
            DELETE_FILE,
            LIST_DIR,
            REINDEX,
            SUBSCRIBE,
            WORKSPACE_INIT,
            WORKSPACE_SELECT,
            WORKSPACE_CURRENT,
        ] {
            assert!(name.starts_with("workspace."), "{name}");
        }
        assert_eq!(EVENT, "workspace.event");
    }
}

/// Start syncing the current workspace.
pub const SYNC_ENABLE: &str = "sync.enable";
/// Stop syncing the current workspace. Files stay.
pub const SYNC_DISABLE: &str = "sync.disable";
/// Report the current workspace's replication state.
pub const SYNC_STATUS: &str = "sync.status";

/// Place a workspace's directory, by name or id.
pub const SYNC_MAP: &str = "sync.map";

/// Parameters of [`SYNC_MAP`]. The mapped directory becomes the current workspace.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncMapParams {
    /// The workspace, by name or id, as the workgroup lists it.
    pub workspace: String,
    /// Where the workspace's root is on this host.
    pub dir: PathBuf,
}

/// Result of [`SYNC_ENABLE`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncEnableResult {
    /// The workspace's identity across devices.
    pub workspace_id: grain_id::GrainId,
}

/// How a synced workspace is wired to its peers right now.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Topology {
    /// Every device syncs with every other. Also the answer when no device was elected.
    #[default]
    Mesh,
    /// Devices that are neither primary nor secondary sync only with those two.
    Star {
        /// The primary device.
        primary: grain_id::GrainId,
        /// The secondary device, if there is a second candidate.
        secondary: Option<grain_id::GrainId>,
    },
}

/// Result of [`SYNC_STATUS`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncStatusResult {
    /// Whether this workspace is synced.
    pub enabled: bool,
    /// Its identity across devices, when it is.
    pub workspace_id: Option<grain_id::GrainId>,
    /// How many other devices the workgroup has.
    pub peers: usize,
    /// Why replication is paused, if it is.
    pub paused: Option<String>,
    /// The last failure, if any.
    pub last_error: Option<String>,
    /// Whether the bridge is reachable. `false` does not mean the app server is down.
    pub bridge_available: bool,
    /// How the workspace is wired to its peers. Absent from older servers: a mesh.
    #[serde(default)]
    pub topology: Topology,
    /// How far embedding has got, when this host embeds (#188).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingProgress>,
}

/// How far a workspace's embedding has got on this host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct EmbeddingProgress {
    /// Documents with a vector.
    pub vectors: u64,
    /// Documents still without one.
    pub pending: u64,
    /// Whether an embedding pass is running now.
    pub running: bool,
}

impl SyncStatusResult {
    /// The state of a workspace nothing syncs.
    pub fn not_synced() -> SyncStatusResult {
        SyncStatusResult {
            enabled: false,
            workspace_id: None,
            peers: 0,
            paused: None,
            last_error: None,
            bridge_available: false,
            topology: Topology::Mesh,
            embedding: None,
        }
    }
}

/// The typed answer to a status question, shared by the CLI and the IPC `server.info`
/// response (spec decision 4).
///
/// When a server answers, the CLI prints the framework's fields and then the
/// application's [rows](StatusReport::app) as `name: value` lines; a GUI could read the
/// same serialised shape from the IPC method instead. When nothing is listening, the
/// report is [`StatusReport::running`] = `false` and the app rows are skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    /// Whether a server is answering at all.
    pub running: bool,
    /// The server's version, when it is running.
    pub version: Option<String>,
    /// Its pid, when it is running.
    pub pid: Option<u32>,
    /// How the running server was started, when it is running.
    pub managed_by: Option<sapphire_ipc::ManagedBy>,
    /// The application's own rows, rendered after the framework's.
    pub app: Vec<StatusRow>,
}

/// One application-provided line of the status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRow {
    /// The row's name, e.g. `sync`.
    pub name: String,
    /// The value shown beside it.
    pub value: String,
}
