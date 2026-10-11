//! The app server's side of the bridge's control plane: a typed client over
//! `sapphire-framework-ipc`.
//!
//! The wire types are in `sapphire-framework-bridge-api`, which carries the bridge's API
//! version and depends on nothing of the framework's. This crate is the part that does:
//! it moves with the framework's version, and the API crate stays put.

#![warn(missing_docs)]

mod client;
pub use client::BridgeClient;

/// Send `header`, read the acknowledgement, and hand back the stream ready for raw bytes.
pub async fn handshake_data<S>(
    mut stream: S,
    header: sapphire_bridge_api::DataHeader,
) -> sapphire_ipc::Result<S>
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
    let ack: sapphire_bridge_api::DataAck = serde_json::from_str(answer.trim())?;
    if !ack.ok {
        return Err(sapphire_ipc::Error::Protocol(
            ack.error
                .unwrap_or_else(|| "the bridge refused the stream".to_owned()),
        ));
    }
    Ok(stream)
}
