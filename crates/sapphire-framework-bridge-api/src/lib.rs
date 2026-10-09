//! What an app server says to the bridge, and how it says it.
//!
//! Kept separate from `sapphire-framework-bridge` so that an app server can talk to the
//! bridge without linking iroh: Cargo unifies features across a workspace build, so a
//! feature flag on one crate would not have been enough.
//!
//! See `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §4.3 and §5.

#![warn(missing_docs)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use grain_id::GrainId;
pub use sapphire_ipc::ManagedBy;

mod client;
pub use client::BridgeClient;

mod embed;
pub use embed::{
    ApiKey, DEFAULT_DIMENSION, DEFAULT_MAX_TOKENS, DeviceSettings, EmbedDeviceSetParams,
    EmbedKeySetParams, EmbedModelSetParams, EmbedNote, EmbedRequest, EmbedSettingsResult,
    LOCAL_MODEL, LocalModel, MAX_TOKENS, ModelSettings, ModelSource, RemoteModel, Slot, SlotModel,
    describe,
};

#[cfg(feature = "cli")]
mod cli;
#[cfg(feature = "cli")]
pub use cli::{EmbeddingCommand, KeyCommand, LocalCommand, RemoteCommand, Switch, read_key};

/// The version of the bridge's control-plane API: the methods below and their types.
///
/// It is this crate's major version, parsed at compile time, so the two cannot drift: a
/// breaking change to a method, a parameter or a result here is a major release of this
/// crate, and that release is the new API version. The handshake compares this number.
pub const API_VERSION: u32 = parse_major(env!("CARGO_PKG_VERSION_MAJOR"));

/// `CARGO_PKG_VERSION_MAJOR` as a number. Cargo guarantees it is decimal digits.
const fn parse_major(s: &str) -> u32 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut n = 0u32;
    while i < bytes.len() {
        n = n * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    n
}

/// The endpoint name the bridge's control plane listens under.
pub const BRIDGE_NAME: &str = "bridge";
/// The endpoint name the bridge's data plane listens under.
pub const BRIDGE_DATA_NAME: &str = "bridge-data";
/// The application-layer protocol name used on every peer connection.
pub const ALPN: &[u8] = b"sapphire/ws/1";

/// Announce which workspaces this app server owns.
pub const REGISTER: &str = "bridge.register";
/// Stop owning one workspace.
pub const UNREGISTER: &str = "bridge.unregister";
/// List the workgroup's devices and whether they are connected.
pub const PEERS: &str = "bridge.peers";
/// Describe the bridge.
pub const STATUS: &str = "bridge.status";
/// Notification: a peer wants a workspace this app server owns.
pub const INCOMING: &str = "bridge.incoming";
/// Create an invite, and get the ticket to hand to the joining device.
pub const INVITE: &str = "bridge.invite";
/// Join the workgroup a ticket names.
pub const JOIN: &str = "bridge.join";
/// List the workspaces the workgroup knows about.
pub const WORKSPACES: &str = "bridge.workspaces";
/// Found a workgroup on this host, as its first device.
pub const WORKGROUP_CREATE: &str = "bridge.workgroup_create";
/// Retire a device of this host's workgroup.
pub const DEVICE_RETIRE: &str = "bridge.device_retire";
/// Ask the bridge which embedding model it serves, if any.
pub const EMBED_INFO: &str = "embed.info";
/// Embed texts with the bridge's embedding model.
pub const EMBED: &str = "embed.embed";
/// Read the embedding settings and what they resolve to.
pub const EMBED_SETTINGS: &str = "embed.settings";
/// Set or clear one model slot.
pub const EMBED_MODEL_SET: &str = "embed.model_set";
/// Switch one slot on or off on this device.
pub const EMBED_DEVICE_SET: &str = "embed.device_set";
/// Store the remote slot's API key on this device.
pub const EMBED_KEY_SET: &str = "embed.key_set";
/// Remove the remote slot's API key from this device.
pub const EMBED_KEY_CLEAR: &str = "embed.key_clear";

/// Set a device's election priority.
pub const DEVICE_PRIORITY_SET: &str = "bridge.device_priority_set";

/// A device's priority when its record names none. Mirrors the registry's own constant;
/// this crate does not depend on the registry.
pub const DEFAULT_PRIORITY: u8 = 1;

fn default_priority() -> u8 {
    DEFAULT_PRIORITY
}

/// Parameters of [`DEVICE_PRIORITY_SET`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevicePrioritySetParams {
    /// The device's name or id.
    pub selector: String,
    /// `0..=255`. `0` takes the device out of the election.
    pub priority: u8,
}

/// Result of [`DEVICE_PRIORITY_SET`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevicePrioritySetResult {
    /// The device's id.
    pub device_id: GrainId,
    /// Its name.
    pub name: String,
    /// Its priority now.
    pub priority: u8,
}

/// Parameters of [`INVITE`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InviteParams {
    /// What the joining device will be called in the ledger.
    pub name: String,
    /// How long the invite stays good, in seconds. `None` uses the bridge's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
    /// The workgroup to invite into, by name or id. `None` uses this host's only one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workgroup: Option<String>,
}

/// Result of [`INVITE`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InviteResult {
    /// The ticket, in the text form a user copies to the joining device.
    pub ticket: String,
}

/// Parameters of [`WORKGROUP_CREATE`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkgroupCreateParams {
    /// The workgroup's name.
    pub name: String,
    /// This host's device name inside it.
    pub device_name: String,
}

/// Result of [`WORKGROUP_CREATE`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkgroupCreateResult {
    /// The new workgroup's id.
    pub workgroup_id: GrainId,
    /// Its name.
    pub name: String,
    /// This host's device id in it.
    pub device_id: GrainId,
}

/// Parameters of [`DEVICE_RETIRE`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceRetireParams {
    /// The device's name or id.
    pub selector: String,
}

/// Result of [`DEVICE_RETIRE`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceRetireResult {
    /// The retired device's id.
    pub device_id: GrainId,
    /// Its name.
    pub name: String,
}

/// Parameters of [`JOIN`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JoinParams {
    /// The ticket the inviter produced.
    pub ticket: String,
    /// The name this device will carry in the ledger. `None` uses the host name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}

/// Result of [`JOIN`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JoinResult {
    /// The workgroup that was joined.
    pub workgroup_id: GrainId,
    /// Its name, as the founding device chose it.
    pub workgroup_name: String,
    /// This device's own record inside it.
    pub device_id: GrainId,
}

/// One workspace the workgroup knows about.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkgroupWorkspaceInfo {
    /// Its identity across devices.
    pub workspace_id: GrainId,
    /// The application that owns it.
    pub app_name: String,
    /// The name the workgroup lists it under.
    pub name: String,
}

/// Result of [`WORKSPACES`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspacesResult {
    /// Every workspace the workgroup knows about.
    pub workspaces: Vec<WorkgroupWorkspaceInfo>,
}

/// The success payload of a call that returns nothing.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Ack {}

/// One workspace an app server owns.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkspaceRegistration {
    /// The workspace's sync identity, shared across devices.
    pub workspace_id: GrainId,
    /// Where it lives on this host.
    pub root: PathBuf,
}

/// Parameters of [`REGISTER`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RegisterParams {
    /// Which application this server belongs to.
    pub app_name: String,
    /// The executable to run when a peer wants a workspace and this server is not running.
    pub exe_path: PathBuf,
    /// How this server was started. A `Service` server is never started by the bridge.
    pub managed_by: ManagedBy,
    /// The workspaces it owns. Registering again replaces the previous list for this app.
    pub workspaces: Vec<WorkspaceRegistration>,
}

/// Result of [`REGISTER`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RegisterResult {
    /// This host's device id inside the workgroup, used as `Entry.author`.
    pub device_id: GrainId,
    /// This host's iroh node id.
    pub node_id: String,
    /// The workgroup these workspaces belong to.
    pub workgroup_id: GrainId,
}

/// Parameters of [`UNREGISTER`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UnregisterParams {
    /// The workspace to stop owning.
    pub workspace_id: GrainId,
}

/// Who the bridge elected for one workspace this host's app servers own.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkspaceRoles {
    /// The workspace.
    pub workspace_id: GrainId,
    /// The primary device, if any candidate exists.
    #[serde(default)]
    pub primary: Option<GrainId>,
    /// The secondary device, if a second candidate exists.
    #[serde(default)]
    pub secondary: Option<GrainId>,
}

/// One device of the workgroup.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeerInfo {
    /// Its device id.
    pub device_id: GrainId,
    /// Its name.
    pub name: String,
    /// Its iroh node id.
    pub node_id: String,
    /// Whether the bridge currently holds a connection to it.
    pub connected: bool,
    /// Its election priority, as its ledger record says.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// Its availability tier, as its own Hello reports it (#190). `None` until measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<u8>,
}

/// Result of [`PEERS`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeersResult {
    /// Every non-retired device of the workgroup, this host included.
    pub peers: Vec<PeerInfo>,
    /// The elected roles of every workspace this host's app servers own. Empty from a
    /// bridge older than 2.1, which means a full mesh.
    #[serde(default)]
    pub roles: Vec<WorkspaceRoles>,
}

impl PeersResult {
    /// The roles for `workspace_id`, if the bridge elected any.
    pub fn roles_for(&self, workspace_id: GrainId) -> Option<&WorkspaceRoles> {
        self.roles.iter().find(|r| r.workspace_id == workspace_id)
    }
}

/// One row of the routing table.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RouteStatus {
    /// The workspace.
    pub workspace_id: GrainId,
    /// The application that owns it.
    pub app_name: String,
    /// Where it lives on this host.
    pub root: PathBuf,
    /// Whether that application's server is connected right now.
    pub owner_online: bool,
}

/// The workgroup this host belongs to.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkgroupStatus {
    /// Its id.
    pub workgroup_id: GrainId,
    /// Its name.
    pub name: String,
    /// How many non-retired devices it has.
    pub devices: usize,
}

/// The embedding model a bridge serves.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EmbedModelInfo {
    /// The model's name.
    pub model: String,
    /// Its vector dimension.
    pub dimension: u32,
    /// The version of the text template applied before embedding.
    pub template_version: u32,
}

/// Result of [`EMBED_INFO`].
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EmbedInfoResult {
    /// Whether this bridge embeds at all.
    pub enabled: bool,
    /// The model, when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<EmbedModelInfo>,
    /// Whether the model is in memory right now (local provider); always true for REST.
    #[serde(default)]
    pub loaded: bool,
    /// Why embedding is off, or what it lacks while on. Absent from bridges before 2.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<EmbedNote>,
}

/// Parameters of [`EMBED`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmbedParams {
    /// The texts to embed.
    pub texts: Vec<String>,
}

/// Result of [`EMBED`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmbedResult {
    /// The model that produced the vectors.
    pub model: String,
    /// Their dimension.
    pub dimension: u32,
    /// One vector per input text, in order.
    pub vectors: Vec<Vec<f32>>,
}

/// Result of [`STATUS`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusResult {
    /// The bridge's version.
    pub version: String,
    /// This host's iroh node id.
    pub node_id: String,
    /// The workgroup, if this host has joined one.
    pub workgroup: Option<WorkgroupStatus>,
    /// Every registered workspace.
    pub routes: Vec<RouteStatus>,
    /// The embedding service, when this bridge reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbedInfoResult>,
}

/// Parameters of the [`INCOMING`] notification.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IncomingParams {
    /// The workspace the peer asked for.
    pub workspace_id: GrainId,
    /// Which device asked.
    pub peer_device_id: GrainId,
    /// A single-use token naming the waiting stream.
    ///
    /// Useless to anyone who did not receive this notification, and consumed the first time
    /// it is presented.
    pub ticket: String,
}

/// The first line of a data-plane connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum DataHeader {
    /// The app server wants a stream to `device_id` for `workspace_id`.
    Open {
        /// The workspace being synced.
        workspace_id: GrainId,
        /// The peer to reach.
        device_id: GrainId,
    },
    /// The app server is answering a [`INCOMING`] notification.
    Accept {
        /// The ticket from that notification.
        ticket: String,
    },
}

/// The bridge's answer to a [`DataHeader`], sent as one line before the raw bytes begin.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DataAck {
    /// Whether the stream is open.
    pub ok: bool,
    /// Why not, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Send `header`, read the acknowledgement, and hand back the stream ready for raw bytes.
pub async fn handshake_data<S>(mut stream: S, header: DataHeader) -> sapphire_ipc::Result<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut line = serde_json::to_vec(&header)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;

    // Read exactly one line without buffering past it, so the raw bytes that follow stay on
    // the stream.
    let mut reader = BufReader::with_capacity(1, &mut stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).await?;
    let ack: DataAck = serde_json::from_str(answer.trim())?;
    if !ack.ok {
        return Err(sapphire_ipc::Error::Protocol(
            ack.error
                .unwrap_or_else(|| "the bridge refused the stream".to_owned()),
        ));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(v: &T) -> T {
        serde_json::from_value(serde_json::to_value(v).unwrap()).unwrap()
    }

    fn id() -> GrainId {
        GrainId::random()
    }

    #[test]
    fn embed_info_round_trips() {
        let info = EmbedInfoResult {
            enabled: true,
            model: Some(EmbedModelInfo {
                model: "m".into(),
                dimension: 384,
                template_version: 1,
            }),
            loaded: true,
            note: Some(EmbedNote::KeyMissing),
        };
        let back = round_trip(&info);
        assert_eq!(back.note, Some(EmbedNote::KeyMissing));
        assert!(back.enabled && back.loaded);
        assert_eq!(back.model, info.model);
        let off = serde_json::to_value(EmbedInfoResult::default()).unwrap();
        assert_eq!(off, serde_json::json!({"enabled": false, "loaded": false}));
    }

    #[test]
    fn status_without_embedding_deserializes() {
        let json = serde_json::json!({
            "version": "1", "node_id": "n", "workgroup": null, "routes": []
        });
        let status: StatusResult = serde_json::from_value(json).unwrap();
        assert!(status.embedding.is_none());
    }

    #[test]
    fn embed_params_wire_shape() {
        let v = serde_json::to_value(EmbedParams {
            texts: vec!["a".into()],
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({"texts": ["a"]}));
        let r = round_trip(&EmbedResult {
            model: "m".into(),
            dimension: 2,
            vectors: vec![vec![1.0, 2.0]],
        });
        assert_eq!(r.vectors, vec![vec![1.0, 2.0]]);
    }

    #[test]
    fn registration_round_trips() {
        let params = RegisterParams {
            app_name: "sapphire-journal".into(),
            exe_path: "/usr/bin/sapphire-journal".into(),
            managed_by: sapphire_ipc::ManagedBy::Spawned,
            workspaces: vec![WorkspaceRegistration {
                workspace_id: id(),
                root: "/home/me/journal".into(),
            }],
        };
        let back = round_trip(&params);
        assert_eq!(back.app_name, "sapphire-journal");
        assert_eq!(back.workspaces.len(), 1);
    }

    #[test]
    fn invite_params_carry_the_cli_arguments() {
        let params = InviteParams {
            name: "phone".into(),
            ttl: Some(600),
            workgroup: None,
        };
        // Serialisation shape is the contract with the bridge's handler; pin it.
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["name"], "phone");
        assert_eq!(json["ttl"], 600);
        assert!(json.get("workgroup").is_none());
    }

    #[test]
    fn workspaces_result_round_trips() {
        let raw = serde_json::json!({ "workspaces": [] });
        let parsed: WorkspacesResult = serde_json::from_value(raw).unwrap();
        assert!(parsed.workspaces.is_empty());
    }

    #[test]
    fn an_open_header_round_trips_and_is_tagged() {
        let header = DataHeader::Open {
            workspace_id: id(),
            device_id: id(),
        };
        let value = serde_json::to_value(&header).unwrap();
        assert_eq!(value["kind"], "open");
        assert_eq!(round_trip(&header), header);
    }

    #[test]
    fn an_accept_header_round_trips_and_is_tagged() {
        let header = DataHeader::Accept {
            ticket: "t-123".into(),
        };
        let value = serde_json::to_value(&header).unwrap();
        assert_eq!(value["kind"], "accept");
        assert_eq!(round_trip(&header), header);
    }

    #[test]
    fn an_unknown_header_kind_is_refused() {
        let value = serde_json::json!({ "kind": "sideways" });
        assert!(serde_json::from_value::<DataHeader>(value).is_err());
    }

    #[test]
    fn a_data_ack_carries_its_reason_when_it_fails() {
        let ack = DataAck {
            ok: false,
            error: Some("no such workspace".into()),
        };
        assert_eq!(round_trip(&ack).error.as_deref(), Some("no such workspace"));
    }

    #[test]
    fn method_names_are_namespaced() {
        for name in [
            REGISTER, UNREGISTER, PEERS, STATUS, INCOMING, INVITE, JOIN, WORKSPACES,
        ] {
            assert!(name.starts_with("bridge."), "{name}");
        }
    }

    #[test]
    fn the_crate_stays_free_of_the_workspace_and_network_stacks() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in [
            "iroh",
            "sapphire-framework-workspace",
            "sapphire-framework-retrieve",
            "sapphire-framework-backend",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "sapphire-framework-bridge-api must not depend on {forbidden}"
            );
        }
    }

    #[test]
    fn the_api_version_is_the_crate_major() {
        let major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap();
        assert_eq!(API_VERSION, major);
        assert_eq!(API_VERSION, 2);
    }

    #[test]
    fn workgroup_create_and_device_retire_wire_shapes() {
        let p = WorkgroupCreateParams {
            name: "home".into(),
            device_name: "desk".into(),
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({ "name": "home", "device_name": "desk" })
        );
        let p = DeviceRetireParams {
            selector: "laptop".into(),
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({ "selector": "laptop" })
        );
        let id = GrainId::random();
        let r = WorkgroupCreateResult {
            workgroup_id: id,
            name: "home".into(),
            device_id: id,
        };
        assert_eq!(round_trip(&r).name, "home");
        assert_eq!(WORKGROUP_CREATE, "bridge.workgroup_create");
        assert_eq!(DEVICE_RETIRE, "bridge.device_retire");
    }

    #[test]
    fn a_2_0_peers_answer_reads_with_no_roles_and_default_priority() {
        let id = GrainId::random();
        let old = serde_json::json!({
            "peers": [{ "device_id": id, "name": "desk", "node_id": "", "connected": true }]
        });

        let read: PeersResult = serde_json::from_value(old).unwrap();

        assert!(read.roles.is_empty());
        assert_eq!(read.peers[0].priority, DEFAULT_PRIORITY);
        assert_eq!(read.peers[0].availability, None);
        assert!(read.roles_for(id).is_none());
    }

    #[test]
    fn roles_for_finds_the_workspace() {
        let ws = GrainId::random();
        let d = GrainId::random();
        let result = PeersResult {
            peers: Vec::new(),
            roles: vec![WorkspaceRoles {
                workspace_id: ws,
                primary: Some(d),
                secondary: None,
            }],
        };

        assert_eq!(result.roles_for(ws).unwrap().primary, Some(d));
    }

    #[test]
    fn device_priority_set_params_are_selector_and_priority() {
        let p = DevicePrioritySetParams {
            selector: "desk".into(),
            priority: 0,
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({ "selector": "desk", "priority": 0 })
        );
    }
}
