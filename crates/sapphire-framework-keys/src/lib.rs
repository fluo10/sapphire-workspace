//! Bearer-token authentication for an application's HTTP routes, against the workgroup's
//! external devices (#199).
//!
//! An external device is a client that reaches the workgroup's applications with a key
//! instead of syncing: a recording pendant, a remote ACP client, a webhook. The bridge keeps
//! the ledger (a token's hash, the applications it may use); this crate asks it.
//!
//! - [`Verifier`]: who a presented token belongs to — [`Verdict::Accepted`], refused, or
//!   that nobody could be asked.
//! - [`BridgeVerifier`]: the verifier an application server uses — `external_device.authenticate`
//!   on the bridge, a success cached for [`CACHE_TTL`].
//! - [`protect`] (feature `axum`): the layer that checks `Authorization: Bearer <token>`
//!   before a request reaches any route, the framework's and the app's own alike.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use sapphire_framework_keys::{AuthConfig, BridgeVerifier};
//! let verifier = Arc::new(BridgeVerifier::new("sapphire-agent", env!("CARGO_PKG_VERSION")));
//! let config = AuthConfig::new(verifier);
//! ```

#[cfg(feature = "axum")]
mod auth;
mod config;
mod verify;

#[cfg(feature = "axum")]
pub use auth::protect;
pub use config::AuthConfig;
pub use verify::{Authenticated, BridgeVerifier, CACHE_TTL, Verdict, Verifier};

// `Authenticated::id`'s type, so an application can name it without depending on grain-id.
pub use grain_id::GrainId;
