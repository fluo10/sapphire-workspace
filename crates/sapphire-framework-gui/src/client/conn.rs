//! Cached connections, and turning failures into [`Conn`] states.

use std::time::Duration;

use sapphire_backend::protocol as proto;
use sapphire_bridge_api::BridgeClient;
use sapphire_ipc::{Client, ClientInfo};

use super::types::{BridgeState, ClientConfig, Conn, ServerState};

/// What a failure means for the display.
pub fn classify<T>(err: &sapphire_ipc::Error) -> Conn<T> {
    match err {
        sapphire_ipc::Error::NotRunning(_) => Conn::Absent,
        sapphire_ipc::Error::ApiVersionMismatch { .. }
        | sapphire_ipc::Error::VersionMismatch { .. } => Conn::Incompatible(err.to_string()),
        other => Conn::Error(other.to_string()),
    }
}

/// The message for a process that did not answer within `limit`.
pub(crate) fn unanswered(process: &str, limit: Duration) -> String {
    if limit.as_millis() < 1000 {
        format!("{process} did not answer within {} ms", limit.as_millis())
    } else {
        format!("{process} did not answer within {} s", limit.as_secs())
    }
}

/// One cached connection per process. A failed call drops the cache, so the next refresh
/// reconnects — and finds the process gone, or back.
#[derive(Default)]
pub(crate) struct Connections {
    bridge: Option<BridgeClient>,
    app: Option<Client>,
}

impl Connections {
    pub(crate) async fn bridge(
        &mut self,
        cfg: &ClientConfig,
    ) -> sapphire_ipc::Result<Option<&BridgeClient>> {
        if self.bridge.is_none() {
            self.bridge =
                BridgeClient::connect_at(&cfg.endpoints.bridge, "gui", cfg.app.version).await?;
        }
        Ok(self.bridge.as_ref())
    }

    pub(crate) async fn app(
        &mut self,
        cfg: &ClientConfig,
    ) -> sapphire_ipc::Result<Option<&Client>> {
        if self.app.is_none() {
            let info = ClientInfo {
                kind: "gui".to_owned(),
                version: cfg.app.version.to_owned(),
                api: proto::API_VERSION,
                pid: std::process::id(),
            };
            self.app = sapphire_ipc::connect_or_absent(&cfg.endpoints.app, cfg.app.app_name, info)
                .await?
                .map(|(client, _)| client);
        }
        Ok(self.app.as_ref())
    }

    pub(crate) fn drop_bridge(&mut self) {
        self.bridge = None;
    }

    pub(crate) fn drop_app(&mut self) {
        self.app = None;
    }

    /// Ask the bridge everything the views show.
    pub(crate) async fn fetch_bridge(&mut self, cfg: &ClientConfig) -> Conn<BridgeState> {
        let result = tokio::time::timeout(cfg.fetch_timeout, async {
            let Some(c) = self.bridge(cfg).await? else {
                return Ok(None);
            };
            let status = c.status().await?;
            let (peers, peer_roles, ledger) = if status.workgroup.is_some() {
                let p = c.peers().await?;
                (p.peers, p.roles, c.workspaces().await?.workspaces)
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };
            // A bridge older than 2.3 has no such method: the screen says so.
            let embedding = c.embed_settings().await.ok();
            // A bridge older than 2.3 has no such method: the screen says so.
            let external_devices = if status.workgroup.is_some() {
                match c
                    .external_device_request(sapphire_bridge_api::ExternalDeviceRequest::List)
                    .await
                {
                    Ok(sapphire_bridge_api::ExternalDeviceOutcome::List(list)) => Some(list),
                    _ => None,
                }
            } else {
                Some(Vec::new())
            };
            Ok::<_, sapphire_ipc::Error>(Some(BridgeState {
                status,
                peers,
                peer_roles,
                ledger,
                embedding,
                external_devices,
            }))
        })
        .await;
        match result {
            Ok(Ok(Some(state))) => Conn::Up(state),
            Ok(Ok(None)) => Conn::Absent,
            Ok(Err(err)) => {
                self.drop_bridge();
                classify(&err)
            }
            Err(_) => {
                self.drop_bridge();
                Conn::Error(unanswered("sapphire-bridge", cfg.fetch_timeout))
            }
        }
    }

    /// Ask the app server everything the views show.
    pub(crate) async fn fetch_server(&mut self, cfg: &ClientConfig) -> Conn<ServerState> {
        let result = tokio::time::timeout(cfg.fetch_timeout, async {
            let Some(c) = self.app(cfg).await? else {
                return Ok(None);
            };
            let info: proto::StatusReport =
                c.call(proto::SERVER_INFO, serde_json::json!({})).await?;
            let list: proto::WorkspaceListResult =
                c.call(proto::WORKSPACE_LIST, serde_json::json!({})).await?;
            Ok::<_, sapphire_ipc::Error>(Some(ServerState {
                info,
                workspaces: list.workspaces,
            }))
        })
        .await;
        match result {
            Ok(Ok(Some(state))) => Conn::Up(state),
            Ok(Ok(None)) => Conn::Absent,
            Ok(Err(err)) => {
                self.drop_app();
                classify(&err)
            }
            Err(_) => {
                self.drop_app();
                Conn::Error(unanswered(cfg.app.app_name, cfg.fetch_timeout))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_api_mismatch_to_incompatible() {
        let err = sapphire_ipc::Error::ApiVersionMismatch {
            running: 1,
            ours: 2,
            server_version: "0.1.0".into(),
        };
        match classify::<()>(&err) {
            Conn::Incompatible(msg) => assert_eq!(msg, err.to_string()),
            other => panic!("expected Incompatible, got {other:?}"),
        }
        let err = sapphire_ipc::Error::VersionMismatch { ours: 1, theirs: 2 };
        assert!(matches!(classify::<()>(&err), Conn::Incompatible(_)));
    }

    #[test]
    fn classify_maps_not_running_to_absent_and_the_rest_to_error() {
        assert!(matches!(
            classify::<()>(&sapphire_ipc::Error::NotRunning("x".into())),
            Conn::Absent
        ));
        assert!(matches!(
            classify::<()>(&sapphire_ipc::Error::Closed),
            Conn::Error(_)
        ));
    }
}
