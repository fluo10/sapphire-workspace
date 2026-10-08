//! The app server's side of the control plane.

use std::sync::Arc;

use sapphire_ipc::{Client, ClientInfo, Endpoint, connect_or_absent};
use tokio::sync::broadcast;

use crate::{
    Ack, BRIDGE_DATA_NAME, BRIDGE_NAME, DEVICE_RETIRE, DataHeader, DeviceRetireParams,
    DeviceRetireResult, EMBED, EMBED_INFO, EmbedInfoResult, EmbedParams, EmbedResult, GrainId,
    INVITE, IncomingParams, InviteParams, InviteResult, JOIN, JoinParams, JoinResult, PEERS,
    PeersResult, REGISTER, RegisterParams, RegisterResult, STATUS, StatusResult, UNREGISTER,
    UnregisterParams, WORKGROUP_CREATE, WORKSPACES, WorkgroupCreateParams, WorkgroupCreateResult,
    WorkspacesResult,
};

/// How many pending incoming announcements a subscriber may fall behind by.
const INCOMING_CAPACITY: usize = 64;

/// An app server's connection to the bridge.
#[derive(Debug)]
pub struct BridgeClient {
    client: Arc<Client>,
    incoming: broadcast::Sender<IncomingParams>,
    runtime_dir: std::path::PathBuf,
}

impl BridgeClient {
    /// Connect to the bridge. Nothing is started: the bridge runs as a service or from a
    /// terminal, and a caller that finds nothing there reports "no bridge is running".
    pub async fn connect(kind: &str, version: &str) -> sapphire_ipc::Result<BridgeClient> {
        let runtime_dir = sapphire_ipc::runtime_dir()?;
        let endpoint = Endpoint::in_dir(BRIDGE_NAME, runtime_dir.clone());
        let info = ClientInfo {
            kind: kind.to_owned(),
            version: version.to_owned(),
            api: crate::API_VERSION,
            pid: std::process::id(),
        };
        let (client, _) = connect_or_absent(&endpoint, BRIDGE_NAME, info)
            .await?
            .ok_or_else(|| {
                sapphire_ipc::Error::NotRunning(
                    "no bridge is running; start it with `sapphire-bridge serve` \
                     or install its service"
                        .to_owned(),
                )
            })?;
        Ok(BridgeClient::from_client(Arc::new(client), runtime_dir))
    }

    /// Connect to the bridge listening at `endpoint`, or `None` when nothing listens there.
    ///
    /// For a caller that must not touch the default runtime directory — a GUI under test, or
    /// one pointed at another directory. The data plane is looked up beside the control
    /// endpoint, as the bridge places it.
    pub async fn connect_at(
        endpoint: &Endpoint,
        kind: &str,
        version: &str,
    ) -> sapphire_ipc::Result<Option<BridgeClient>> {
        let info = ClientInfo {
            kind: kind.to_owned(),
            version: version.to_owned(),
            api: crate::API_VERSION,
            pid: std::process::id(),
        };
        Ok(connect_or_absent(endpoint, BRIDGE_NAME, info)
            .await?
            .map(|(client, _)| BridgeClient::from_client(Arc::new(client), endpoint.dir.clone())))
    }

    /// Connect to the running bridge, or fail naming it.
    ///
    /// Like [`connect`](Self::connect), but absence is an error the command layer prints
    /// as "no sapphire-bridge is running", rather than a `None` the caller turns into one.
    /// Asking a question must not bring a daemon up, so nothing is started here either.
    pub async fn connect_running(kind: &str, version: &str) -> sapphire_ipc::Result<BridgeClient> {
        let runtime_dir = sapphire_ipc::runtime_dir()?;
        let endpoint = Endpoint::in_dir(BRIDGE_NAME, runtime_dir.clone());
        if !sapphire_ipc::probe(&endpoint).await? {
            return Err(sapphire_ipc::Error::NotRunning(
                "no sapphire-bridge is running".to_owned(),
            ));
        }
        Self::connect(kind, version).await
    }

    /// Wrap an existing connection. Used by tests and by a caller that already has one.
    pub fn from_client(client: Arc<Client>, runtime_dir: std::path::PathBuf) -> BridgeClient {
        let (incoming, _) = broadcast::channel(INCOMING_CAPACITY);
        let mut notifications = client.notifications();
        let sender = incoming.clone();
        tokio::spawn(async move {
            loop {
                match notifications.recv().await {
                    Ok(n) if n.method == crate::INCOMING => {
                        match serde_json::from_value::<IncomingParams>(n.params) {
                            Ok(params) => {
                                let _ = sender.send(params);
                            }
                            Err(err) => tracing::warn!("malformed bridge.incoming: {err}"),
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "fell behind on bridge announcements");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        BridgeClient {
            client,
            incoming,
            runtime_dir,
        }
    }

    /// Announce the workspaces this app server owns.
    pub async fn register(&self, params: RegisterParams) -> sapphire_ipc::Result<RegisterResult> {
        self.client.call(REGISTER, params).await
    }

    /// Found a workgroup on this host.
    pub async fn workgroup_create(
        &self,
        params: WorkgroupCreateParams,
    ) -> sapphire_ipc::Result<WorkgroupCreateResult> {
        self.client.call(WORKGROUP_CREATE, params).await
    }

    /// Retire a device of this host's workgroup.
    pub async fn device_retire(
        &self,
        params: DeviceRetireParams,
    ) -> sapphire_ipc::Result<DeviceRetireResult> {
        self.client.call(DEVICE_RETIRE, params).await
    }

    /// Stop owning one workspace.
    pub async fn unregister(&self, workspace_id: GrainId) -> sapphire_ipc::Result<()> {
        let _: Ack = self
            .client
            .call(UNREGISTER, UnregisterParams { workspace_id })
            .await?;
        Ok(())
    }

    /// The workgroup's devices.
    pub async fn peers(&self) -> sapphire_ipc::Result<PeersResult> {
        self.client.call(PEERS, serde_json::json!({})).await
    }

    /// What the bridge knows about itself.
    pub async fn status(&self) -> sapphire_ipc::Result<StatusResult> {
        self.client.call(STATUS, serde_json::json!({})).await
    }

    /// Which embedding model the bridge serves, if any.
    pub async fn embed_info(&self) -> sapphire_ipc::Result<EmbedInfoResult> {
        self.client.call(EMBED_INFO, serde_json::json!({})).await
    }

    /// Embed `texts` with the bridge's model; one vector per text, in order.
    pub async fn embed(&self, texts: Vec<String>) -> sapphire_ipc::Result<EmbedResult> {
        self.client.call(EMBED, EmbedParams { texts }).await
    }

    /// Ask the bridge to create an invite, and get the ticket back.
    ///
    /// The bridge composes the ticket, because only the process holding the endpoint knows
    /// the address a joiner must dial.
    pub async fn invite(&self, params: InviteParams) -> sapphire_ipc::Result<InviteResult> {
        self.client.call(INVITE, params).await
    }

    /// Ask the bridge to join the workgroup a ticket names.
    pub async fn join(&self, params: JoinParams) -> sapphire_ipc::Result<JoinResult> {
        self.client.call(JOIN, params).await
    }

    /// The workspaces the workgroup knows about.
    pub async fn workspaces(&self) -> sapphire_ipc::Result<WorkspacesResult> {
        self.client.call(WORKSPACES, serde_json::json!({})).await
    }

    /// Announcements that a peer wants a workspace this server owns.
    ///
    /// Answer each one by calling [`accept_stream`](Self::accept_stream) with its ticket.
    pub fn incoming(&self) -> broadcast::Receiver<IncomingParams> {
        self.incoming.subscribe()
    }

    /// Open a stream to `device` for `workspace`.
    pub async fn open_stream(
        &self,
        workspace_id: GrainId,
        device_id: GrainId,
    ) -> sapphire_ipc::Result<sapphire_ipc::RawStream> {
        self.data(DataHeader::Open {
            workspace_id,
            device_id,
        })
        .await
    }

    /// Claim the stream a [`IncomingParams`] announced.
    pub async fn accept_stream(
        &self,
        ticket: String,
    ) -> sapphire_ipc::Result<sapphire_ipc::RawStream> {
        self.data(DataHeader::Accept { ticket }).await
    }

    async fn data(&self, header: DataHeader) -> sapphire_ipc::Result<sapphire_ipc::RawStream> {
        let endpoint = Endpoint::in_dir(BRIDGE_DATA_NAME, self.runtime_dir.clone());
        let raw = sapphire_ipc::connect_raw(&endpoint).await?;
        crate::handshake_data(raw, header).await
    }
}
