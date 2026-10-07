//! The first exchange on every connection (spec §2.4).

use serde::{Deserialize, Serialize};

/// The API version every method set had before API versions were exchanged, and so the
/// version a peer that sends none is taken to speak.
pub const FIRST_API: u32 = 1;

fn first_api() -> u32 {
    FIRST_API
}

/// Sent by the client as the first frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The client's IPC protocol version.
    pub protocol: u32,
    /// The application whose server the client expects to be talking to.
    pub app: String,
    /// Who is connecting.
    pub client: ClientInfo,
}

/// Identifies the connecting process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// `cli`, `desktop`, `mcp`, …
    pub kind: String,
    /// The client's crate version. Informational: it is shown, never compared.
    pub version: String,
    /// The API version the client expects the server to speak: the version of the method
    /// set behind the endpoint, which the server's owner defines (the bridge's in
    /// `sapphire-framework-bridge-api`, an app server's in `sapphire-framework-backend`).
    /// A client that predates the field spoke [`FIRST_API`].
    #[serde(default = "first_api")]
    pub api: u32,
    /// The client's process id.
    pub pid: u32,
}

/// Sent by the server in answer to [`Hello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The server's IPC protocol version.
    pub protocol: u32,
    /// Who answered.
    pub server: ServerInfo,
}

/// Identifies the serving process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// The server's crate version. Informational: it is shown, never compared. Builds of
    /// different crates (an app and the bridge) or different releases talk to each other
    /// as long as their [`api`](Self::api) agrees.
    pub version: String,
    /// The API version this server speaks; see [`ClientInfo::api`]. A server that
    /// predates the field spoke [`FIRST_API`], which is what lets a newer client keep
    /// talking to an installed service that has not been rebuilt.
    #[serde(default = "first_api")]
    pub api: u32,
    /// The server's process id.
    pub pid: u32,
    /// How the server was started, which decides what a client may do about a version
    /// mismatch (spec §2.6).
    pub managed_by: ManagedBy,
}

/// How a server process came to exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedBy {
    /// Started by the OS service manager. A client must not shut it down.
    Service,
    /// Started on demand by a client. A client may ask it to exit and start a new one.
    Spawned,
}
