//! The replication session two sapphire-framework replicas run over a byte stream.
//!
//! One `Replica` per side, one bidirectional stream, and a framed exchange of version
//! vectors, path updates and content. This crate is the only place that knows the wire
//! format; it neither knows nor cares what carries the bytes.
//!
//! Two shapes of session are built on that exchange:
//!
//! - [`run_session`] — a one-shot catch-up. It returns once both sides are caught up.
//! - [`open_live_session`] — the same catch-up, then a stream that stays open: each side
//!   pushes what it commits and applies what arrives.
//!
//! See `docs/superpowers/specs/2026-09-15-p2p-sync-iroh-design.md` §3.7 and §4.2.

#![warn(missing_docs)]

mod error;
mod frame;
mod live;
mod message;
mod session;

pub use error::{Error, Result};
pub use frame::{Frame, read_frame, read_framed, write_blob, write_control, write_framed};
pub use live::{LiveSession, open_live_session};
pub use message::Message;
pub use session::{HELLO_TIMEOUT, SessionOutcome, run_session};

/// The session format this build speaks. Sent in the first message and checked.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// Largest payload carried inline in an `Updates` page, in bytes. Anything larger is
/// fetched by hash within the same session.
pub const INLINE_LIMIT: usize = 64 * 1024;

/// Longest frame payload accepted, in bytes. Matches the replication core's
/// `DEFAULT_MAX_FILE_SIZE`, and bounds the memory one peer can make the other allocate.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
