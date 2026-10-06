//! Connecting to a server that is already running (spec §2.6).
//!
//! This crate no longer starts anything: an app server is started by `serve` in a
//! terminal or by the OS service manager, and the CLI merely finds it. A socket with
//! nobody behind it is a corpse, not a task — [`probe`] clears it.

use crate::client::Client;
use crate::conn::Connection;
use crate::endpoint::Endpoint;
use crate::error::{Error, Result};
use crate::handshake::{ClientInfo, ServerInfo};
use crate::raw::RawStream;

/// The method a client calls to retire a server of the wrong version.
///
/// This crate only names it, so that both sides agree; `sapphire-framework-server`
/// implements it.
pub const SHUTDOWN_METHOD: &str = "server.shutdown";

/// Connect to `endpoint` using this platform's carrier.
pub async fn connect(endpoint: &Endpoint) -> Result<Connection> {
    #[cfg(unix)]
    {
        crate::unix::connect(endpoint).await
    }
    #[cfg(windows)]
    {
        crate::windows::connect(endpoint).await
    }
}

/// Connect to `endpoint` and hand back the byte stream, unframed.
///
/// The bridge's data plane speaks one JSON line and then raw bytes, so it cannot use
/// [`Connection`](crate::Connection).
pub async fn connect_raw(endpoint: &Endpoint) -> Result<RawStream> {
    #[cfg(unix)]
    {
        crate::unix::connect_raw(endpoint).await
    }
    #[cfg(windows)]
    {
        crate::windows::connect_raw(endpoint).await
    }
}

/// Is a server listening on `endpoint`? Clears a stale Unix socket file as a side effect.
pub async fn probe(endpoint: &Endpoint) -> Result<bool> {
    #[cfg(unix)]
    {
        crate::unix::probe(endpoint).await
    }
    #[cfg(windows)]
    {
        crate::windows::probe(endpoint).await
    }
}

/// Handshake with the server on `endpoint`, or report that nothing is there.
///
/// `Ok(None)` covers both "nothing is listening" and "a corpse socket": `probe` has
/// already cleared the latter. A server of a different API version is an error, not a
/// replacement — start-on-demand retired spawned servers by re-exec, and this crate no
/// longer starts anything. It is reported as [`Error::ApiVersionMismatch`]: the caller
/// upgrades and restarts the service, and no client fixes it. A different crate version
/// alone is no mismatch.
pub async fn connect_or_absent(
    endpoint: &Endpoint,
    app: &str,
    client: ClientInfo,
) -> Result<Option<(Client, ServerInfo)>> {
    if !probe(endpoint).await? {
        return Ok(None);
    }
    // The API gate lives in the handshake itself: a server of another API version is
    // [`Error::ApiVersionMismatch`], whose advice is to upgrade and restart. Nothing
    // replaces a server any more.
    match Client::handshake(connect(endpoint).await?, app, client).await {
        Ok(pair) => Ok(Some(pair)),
        // A half-open socket, or a server shutting down: nothing to connect to.
        Err(Error::Io(_) | Error::Closed) => Ok(None),
        Err(err) => Err(err),
    }
}
