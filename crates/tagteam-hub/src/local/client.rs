//! Small local API client used by the CLI and the future stdio MCP shim.
//!
//! Every request carries a caller-generated numeric ID. The client never retries
//! a mutation automatically because command and transition idempotency belongs
//! to the daemon's durable records.

use std::{
    io,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use futures_util::{SinkExt, StreamExt};
use tagteam_core::local_api::{LocalFrame, LocalRequest, LocalResponseBody};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use super::codec;

/// Local client failure.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("could not connect to the Tag Team daemon: {0}")]
    Connect(#[source] io::Error),
    #[error("local socket I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("local frame is malformed")]
    MalformedFrame,
    #[error("daemon request failed: {0}")]
    Daemon(String),
    #[error("daemon closed before replying")]
    Closed,
}

/// Connected length-delimited local API client.
pub struct LocalClient {
    framed: Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    next_id: AtomicU64,
}

impl LocalClient {
    /// Connects to the daemon's private Unix socket.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(ClientError::Connect)?;
        Ok(Self {
            framed: Framed::new(stream, codec()),
            next_id: AtomicU64::new(1),
        })
    }

    /// Sends a request and waits for the matching response or error frame.
    pub async fn request(
        &mut self,
        request: LocalRequest,
    ) -> Result<LocalResponseBody, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(id, request).await?;
        loop {
            let frame = self.next_frame().await?.ok_or(ClientError::Closed)?;
            match frame {
                LocalFrame::Response { id: got, response } if got == id => return Ok(response),
                LocalFrame::Error { id: got, message } if got == id => {
                    return Err(ClientError::Daemon(message));
                }
                LocalFrame::Event { .. } => {}
                LocalFrame::Request { .. } => return Err(ClientError::MalformedFrame),
                LocalFrame::Response { .. } | LocalFrame::Error { .. } => {
                    return Err(ClientError::MalformedFrame);
                }
            }
        }
    }

    /// Sends one request without waiting for a reply.
    pub async fn send(&mut self, id: u64, request: LocalRequest) -> Result<(), ClientError> {
        self.send_frame(LocalFrame::Request { id, request }).await
    }

    /// Sends any locally encoded frame.
    pub async fn send_frame(&mut self, frame: LocalFrame) -> Result<(), ClientError> {
        let bytes = serde_json::to_vec(&frame).map_err(|_| ClientError::MalformedFrame)?;
        self.framed.send(bytes.as_slice()).await?;
        Ok(())
    }

    /// Reads the next frame, or `None` after a clean peer close.
    pub async fn next_frame(&mut self) -> Result<Option<LocalFrame>, ClientError> {
        let Some(bytes) = self.framed.next().await else {
            return Ok(None);
        };
        let bytes = bytes?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| ClientError::MalformedFrame)
    }
}
