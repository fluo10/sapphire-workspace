//! `sapphire-framework` — a single-dependency facade over the local-first
//! framework crates.
//!
//! Depend on this one crate and enable the features you need; each feature
//! re-exports an internal `sapphire-framework-*` crate as a module. This mirrors
//! the way large Rust libraries (e.g. bevy) ship a facade over many internal
//! crates: the split crates keep compile-time isolation, while consumers depend
//! on one name.
//!
//! ```toml
//! # A native app that indexes a local workspace and talks to its app server:
//! sapphire-framework = { version = "0.1", features = ["native", "redb-store"] }
//! ```
//!
//! ```ignore
//! // (requires the `backend` feature)
//! use sapphire_framework::prelude::*;
//! let backend = LocalBackend::new(state);
//! ```
//!
//! ## Modules (feature-gated)
//!
//! | feature | module | crate |
//! |---|---|---|
//! | `workspace` | [`workspace`] | `sapphire-framework-workspace` |
//! | `retrieve` | [`retrieve`] | `sapphire-framework-retrieve` |
//! | `track` | [`track`] | `sapphire-framework-track` |
//! | `sync` | [`sync`] | `sapphire-framework-sync` |
//! | `session` | [`session`] | `sapphire-framework-session` |
//! | `ipc` | [`ipc`] | `sapphire-framework-ipc` |
//! | `server` | [`server`] | `sapphire-framework-server` |
//! | `keys` | [`keys`] | `sapphire-framework-keys` |
//! | `registry` | [`registry`] | `sapphire-framework-registry` |
//! | `bridge` | [`bridge`] | `sapphire-framework-bridge` |
//! | `bridge-embed` | [`bridge_embed`] | `sapphire-framework-bridge-embed` (for apps that embed the bridge in-process, e.g. mobile; not in `native`) |
//! | `backend` | [`backend`] | `sapphire-framework-backend` |
//! | `gui` | [`gui`] | `sapphire-framework-gui` |
//! | `service` | [`service`] | `sapphire-framework-service` |

#[cfg(feature = "workspace")]
pub use sapphire_framework_workspace as workspace;

// The dependency re-exports ride in on the `workspace` feature: apps depend on
// this facade alone and get the same `clap`/`serde`/`dirs` versions the
// workspace crate builds `WorkspaceArgs` and the directory helpers with
// (issue #128).
#[cfg(feature = "workspace")]
pub use sapphire_framework_workspace::{clap, dirs, serde};

#[cfg(feature = "retrieve")]
pub use sapphire_framework_retrieve as retrieve;

#[cfg(feature = "track")]
pub use sapphire_framework_track as track;

#[cfg(feature = "sync")]
pub use sapphire_framework_sync as sync;

#[cfg(feature = "session")]
pub use sapphire_framework_session as session;

#[cfg(feature = "ipc")]
pub use sapphire_framework_ipc as ipc;

#[cfg(feature = "server")]
pub use sapphire_framework_server as server;

#[cfg(feature = "keys")]
pub use sapphire_framework_keys as keys;

#[cfg(feature = "registry")]
pub use sapphire_framework_registry as registry;

#[cfg(feature = "bridge")]
pub use sapphire_framework_bridge as bridge;

#[cfg(feature = "bridge-embed")]
pub use sapphire_framework_bridge_embed as bridge_embed;

#[cfg(feature = "backend")]
pub use sapphire_backend as backend;

#[cfg(feature = "gui")]
pub use sapphire_framework_gui as gui;

#[cfg(feature = "service")]
pub use sapphire_framework_service as service;

/// Commonly-used types, re-exported for `use sapphire_framework::prelude::*;`.
///
/// What is available depends on the enabled features.
pub mod prelude {
    #[cfg(feature = "workspace")]
    pub use crate::workspace::{
        AppContext, AppKind, FileSearchResult, RetrieveConfig, RetrieveParams, SearchMode,
        Workspace, WorkspaceArgs, WorkspaceState,
    };

    #[cfg(feature = "backend")]
    pub use crate::backend::{
        BackendEvent, LocalBackend, SyncSummary, WorkspaceBackend, WorkspaceEntry,
        WorkspaceLocator, WorkspaceRegistry, WorkspaceSelection, WorkspaceSource,
    };

    // A UI leaves the cache to the app server: `IpcBackend` implements
    // `WorkspaceBackend` over IPC, and `protocol` names the `workspace.*`
    // methods and their types for both sides of the connection.
    #[cfg(feature = "backend")]
    pub use crate::backend::{IpcBackend, protocol};

    // `KeyStore` and friends live in the `keys` crate (#103); the `keys`
    // feature chains the crate's `axum` feature in, so an application hosting
    // its own authenticated routes gets `protect`/`Authenticated` from here.
    #[cfg(feature = "keys")]
    pub use crate::keys::{AuthConfig, Authenticated, KeyEntry, KeyStore, protect};

    // `registry` and `keys` both expose the same `GrainId` (`KeyEntry::device_id`
    // lives in the key file). Re-exporting both would collide, so take it from
    // `registry` when that feature is on, and from `keys` only when it is not.
    #[cfg(all(feature = "keys", not(feature = "registry")))]
    pub use crate::keys::GrainId;

    // The app server skeleton: an application builds one of these, adds its own
    // methods, and runs it. `WorkspaceHost` is here for handlers that reach a
    // workspace the same way the framework's own do. `FrameworkCommand` is the
    // flat command vocabulary an app flattens into its own CLI beside its own
    // subcommands (issue #142); the sub-enums and the typed status report come
    // with it, so an app never needs the `server` module to build its CLI.
    #[cfg(feature = "server")]
    pub use sapphire_framework_server::{
        AppServer, DeviceCommand, FrameworkCommand, StatusReport, StatusRow, WorkgroupCommand,
        WorkspaceCommand, WorkspaceHost,
    };
}

#[cfg(all(test, feature = "server"))]
mod tests {
    //! Pin the prelude's server-side surface (issue #142, plan Task 5).

    use super::prelude::*;

    #[test]
    fn the_prelude_exports_the_whole_framework_command_surface() {
        // The flat command vocabulary an app flattens beside its own verbs.
        let _serve: FrameworkCommand = FrameworkCommand::Serve;
        let _status: FrameworkCommand = FrameworkCommand::Status;
        let _workspace: FrameworkCommand = FrameworkCommand::Workspace(WorkspaceCommand::List);
        let _workgroup: FrameworkCommand = FrameworkCommand::Workgroup(WorkgroupCommand::List);
        let _device: FrameworkCommand = FrameworkCommand::Device(DeviceCommand::List);

        // The typed status report shared by the CLI and the IPC `server.info`.
        let _report = StatusReport {
            running: false,
            version: None,
            pid: None,
            managed_by: None,
            app: vec![StatusRow {
                name: "sync".to_owned(),
                value: "enabled".to_owned(),
            }],
        };
    }
}
