//! Local inter-process transport for sapphire-framework applications.
//!
//! A sapphire app's cache is a redb database, which only one process may open. The app's
//! **server** is therefore the only process that touches it, and its CLI, stdio MCP server
//! and desktop UI reach it through this crate: framed JSON-RPC 2.0 over a Unix domain
//! socket, a Windows named pipe, or an in-process channel.
//!
//! See `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §2.
//!
//! This crate is a transport. It knows nothing about workspaces, search or sync, so the
//! bridge — which has no search stack — can use it too.

#![warn(missing_docs)]

mod client;
mod conn;
mod endpoint;
mod error;
mod handshake;
mod message;
mod raw;
mod router;
mod spawn;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub use client::{Client, NOTIFICATION_CAPACITY};
pub use conn::{Connection, Sender};
pub use endpoint::{BRIDGE_NAME, Endpoint, RUNTIME_DIR_ENV, runtime_dir};
pub use error::{Error, Result};
pub use handshake::{ClientInfo, FIRST_API, Hello, ManagedBy, ServerInfo, Welcome};
pub use message::{Message, Notification, Request, Response, ResponsePayload, RpcError, codes};
pub use raw::{RawIo, RawStream};
pub use router::{HANDSHAKE_METHOD, HandlerFuture, PeerHandle, RequestCtx, Router, serve};
pub use spawn::{SHUTDOWN_METHOD, connect, connect_or_absent, connect_raw, probe};
#[cfg(unix)]
pub use unix::{UnixListenerHandle, bind, peer_uid};
#[cfg(windows)]
pub use windows::{PipeListener, bind, current_user_sid};

/// The IPC protocol version this build speaks. Bumped on a breaking change to the
/// envelope or the handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest frame accepted, in bytes. Frames carry file content as JSON strings, and this
/// bounds the memory one hostile or buggy peer can make the other allocate.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
