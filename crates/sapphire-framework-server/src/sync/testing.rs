//! A bridge that records what it was told, for testing `SyncRuntime` without a real one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sapphire_bridge_api::{
    Ack, BridgeClient, GrainId, IncomingParams, PeersResult, RegisterParams, RegisterResult,
    StatusResult, UnregisterParams, WORKSPACES, WorkgroupWorkspaceInfo, WorkspacesResult,
};
use sapphire_ipc::{ClientInfo, Connection, ManagedBy, Router, ServerInfo, serve};

/// What a `StubBridge` saw.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// Every `bridge.register` it received, in order.
    pub registrations: Vec<RegisterParams>,
    /// Every `bridge.unregister` it received.
    pub unregistrations: Vec<GrainId>,
}

/// A stand-in for the bridge's control plane.
pub struct StubBridge {
    /// What it has been told.
    pub seen: Arc<Mutex<Seen>>,
    /// The device id it answers registrations with.
    pub device_id: GrainId,
    /// The workgroup id it answers registrations with.
    pub workgroup_id: GrainId,
    /// The workspaces it answers `bridge.workspaces` with.
    pub listed: Arc<Mutex<Vec<WorkgroupWorkspaceInfo>>>,
    /// How many `bridge.peers` it has been asked for.
    ///
    /// Dials and status reports both ask; a test that watches this grow after touching a
    /// file has proof the runtime was driven, without reaching into a replica.
    peers_queries: Arc<AtomicUsize>,
    announcer: sapphire_ipc::Sender,
}

impl StubBridge {
    /// Start a stub on one end of an in-process connection and return a client for the other.
    pub async fn start() -> (StubBridge, Arc<BridgeClient>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let device_id = GrainId::random();
        let workgroup_id = GrainId::random();
        let peers_queries = Arc::new(AtomicUsize::new(0));
        let listed = Arc::new(Mutex::new(Vec::new()));

        let (client_conn, server_conn) = Connection::pair();
        let announcer = server_conn.sender();

        let router = Arc::new(
            Router::new()
                .method(sapphire_bridge_api::REGISTER, {
                    let seen = Arc::clone(&seen);
                    move |ctx| {
                        let seen = Arc::clone(&seen);
                        async move {
                            let params: RegisterParams = serde_json::from_value(ctx.params)
                                .map_err(|e| {
                                    sapphire_ipc::RpcError::invalid_params(e.to_string())
                                })?;
                            seen.lock().expect("stub").registrations.push(params);
                            serde_json::to_value(RegisterResult {
                                device_id,
                                node_id: "stub".into(),
                                workgroup_id,
                            })
                            .map_err(|e| sapphire_ipc::RpcError::internal(e.to_string()))
                        }
                    }
                })
                .method(sapphire_bridge_api::UNREGISTER, {
                    let seen = Arc::clone(&seen);
                    move |ctx| {
                        let seen = Arc::clone(&seen);
                        async move {
                            let params: UnregisterParams = serde_json::from_value(ctx.params)
                                .map_err(|e| {
                                    sapphire_ipc::RpcError::invalid_params(e.to_string())
                                })?;
                            seen.lock()
                                .expect("stub")
                                .unregistrations
                                .push(params.workspace_id);
                            serde_json::to_value(Ack {})
                                .map_err(|e| sapphire_ipc::RpcError::internal(e.to_string()))
                        }
                    }
                })
                .method(sapphire_bridge_api::PEERS, {
                    let peers_queries = Arc::clone(&peers_queries);
                    move |_| {
                        let peers_queries = Arc::clone(&peers_queries);
                        async move {
                            peers_queries.fetch_add(1, Ordering::Relaxed);
                            serde_json::to_value(PeersResult { peers: vec![], roles: vec![] })
                                .map_err(|e| sapphire_ipc::RpcError::internal(e.to_string()))
                        }
                    }
                })
                .method(WORKSPACES, {
                    let listed = Arc::clone(&listed);
                    move |_| {
                        let listed = Arc::clone(&listed);
                        async move {
                            serde_json::to_value(WorkspacesResult {
                                workspaces: listed.lock().expect("stub").clone(),
                            })
                            .map_err(|e| sapphire_ipc::RpcError::internal(e.to_string()))
                        }
                    }
                })
                .method(sapphire_bridge_api::STATUS, |_| async move {
                    serde_json::to_value(StatusResult {
                        version: "stub".into(),
                        node_id: "stub".into(),
                        workgroup: None,
                        routes: vec![],
                    })
                    .map_err(|e| sapphire_ipc::RpcError::internal(e.to_string()))
                }),
        );

        tokio::spawn(async move {
            let info = ServerInfo {
                version: "stub".into(),
                api: sapphire_bridge_api::API_VERSION,
                pid: std::process::id(),
                managed_by: ManagedBy::Service,
            };
            let _ = serve(server_conn, router, "bridge", info).await;
        });

        let info = ClientInfo {
            kind: "test".into(),
            version: "stub".into(),
            api: sapphire_bridge_api::API_VERSION,
            pid: std::process::id(),
        };
        let (client, _) = sapphire_ipc::Client::handshake(client_conn, "bridge", info)
            .await
            .expect("the stub handshake");
        let client = Arc::new(BridgeClient::from_client(
            Arc::new(client),
            std::env::temp_dir(),
        ));

        (
            StubBridge {
                seen,
                listed,
                device_id,
                workgroup_id,
                peers_queries,
                announcer,
            },
            client,
        )
    }

    /// Pretend a peer wants `workspace_id`.
    pub async fn announce(&self, workspace_id: GrainId, ticket: &str) {
        let params = IncomingParams {
            workspace_id,
            peer_device_id: GrainId::random(),
            ticket: ticket.to_owned(),
        };
        let _ = self
            .announcer
            .send(sapphire_ipc::Message::Notification(
                sapphire_ipc::Notification {
                    method: sapphire_bridge_api::INCOMING.to_owned(),
                    params: serde_json::to_value(params).expect("announcement"),
                },
            ))
            .await;
    }

    /// How many `bridge.peers` calls it has answered.
    pub fn peers_queries(&self) -> usize {
        self.peers_queries.load(Ordering::Relaxed)
    }

    /// Publish `info` as a workspace the workgroup knows about.
    pub fn list(&self, info: WorkgroupWorkspaceInfo) {
        self.listed.lock().expect("stub").push(info);
    }

    /// The last registration's workspace list.
    pub fn last_workspaces(&self) -> Vec<GrainId> {
        self.seen
            .lock()
            .expect("stub")
            .registrations
            .last()
            .map(|r| r.workspaces.iter().map(|w| w.workspace_id).collect())
            .unwrap_or_default()
    }
}
