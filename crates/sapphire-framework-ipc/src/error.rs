use thiserror::Error;

use crate::message::RpcError;

/// Errors surfaced by the IPC layer.
#[derive(Debug, Error)]
pub enum Error {
    /// Socket, pipe or file-system failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A frame was not valid JSON, or did not deserialise into the expected shape.
    #[error("malformed frame: {0}")]
    Codec(#[from] serde_json::Error),

    /// A frame was syntactically valid but not a legal message here (for example a
    /// response to an id that was never sent, or a string request id).
    #[error("protocol violation: {0}")]
    Protocol(String),

    /// A frame exceeded [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN).
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge {
        /// Length that was announced or read.
        len: usize,
        /// The configured limit.
        max: usize,
    },

    /// The two ends do not speak the same protocol version.
    #[error("protocol version mismatch: this process speaks {ours}, the peer speaks {theirs}")]
    VersionMismatch {
        /// This process's version.
        ours: u32,
        /// The peer's version.
        theirs: u32,
    },

    /// The peer is not the same OS user, and was disconnected.
    #[error("rejected a connection from another user")]
    PeerRejected,

    /// The connection closed before the operation finished.
    #[error("connection closed")]
    Closed,

    /// The peer answered the call with a JSON-RPC error.
    #[error("remote error {}: {}", .0.code, .0.message)]
    Rpc(RpcError),

    /// A server did not appear, or did not answer, within the time allowed.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),

    /// The process this caller meant to talk to is not running.
    ///
    /// The payload is the sentence a caller prints or wraps: it names the process and,
    /// where starting one is the answer, the command that starts it (`serve`, or the
    /// service manager). Start-on-demand is gone (2026-09-24 spec decision 3), so
    /// nothing in this crate — and nothing behind this error — starts a process.
    #[error("{0}")]
    NotRunning(String),

    /// A running server speaks a different API version than this process expects.
    ///
    /// Only the API is compared, never the crate version: an app and the bridge are
    /// different crates, and a patch release of either one must not lock the other out.
    /// The running server is not replaced, because the OS service manager owns it, so the
    /// fix is to upgrade whichever side is older and restart it.
    #[error(
        "the running service (version {server_version}) speaks API v{running}, this \
         process speaks API v{ours}; upgrade whichever is older and restart the service"
    )]
    ApiVersionMismatch {
        /// API version reported by the running server.
        running: u32,
        /// API version this process expects.
        ours: u32,
        /// Crate version reported by the running server, for the message.
        server_version: String,
    },
}

/// Convenience alias for IPC results.
pub type Result<T> = std::result::Result<T, Error>;
