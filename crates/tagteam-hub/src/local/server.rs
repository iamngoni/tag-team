//! Unix-socket daemon for CLI and MCP requests.
//!
//! The socket lives inside a 0700 data directory and is chmodded to 0600 after
//! bind. Every accepted connection is checked against the daemon's effective UID.

use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
};

use futures_util::{SinkExt, StreamExt};
use nix::unistd::geteuid;
use tagteam_core::local_api::{LocalFrame, LocalRequest, LocalResponseBody, WatchScope};
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{mpsc, watch},
    task::JoinSet,
};
use tokio_util::codec::Framed;

use crate::service::{HubService, ServiceError};

use super::codec;

/// Local daemon lifecycle failure.
#[derive(Debug, Error)]
pub enum LocalServerError {
    #[error("local socket I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("local socket path is already in use")]
    AlreadyRunning,
    #[error("local socket path is not a Unix socket")]
    UnsafeSocketPath,
    #[error("local socket request handler stopped")]
    Worker,
    #[error("local frame is malformed or exceeds the 1 MiB limit")]
    MalformedFrame,
    #[error("local peer is running as a different user")]
    WrongPeer,
}

/// Serves local API connections until shutdown is requested.
pub async fn serve(
    socket_path: impl AsRef<Path>,
    service: HubService,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), LocalServerError> {
    let socket_path = socket_path.as_ref().to_path_buf();
    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    let cleanup = SocketCleanup(socket_path.clone());
    let expected_uid = geteuid().as_raw();
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let peer_uid = match stream.peer_cred() {
                    Ok(credentials) => credentials.uid(),
                    Err(error) => {
                        // A peer can close before the daemon inspects its credentials.
                        // Reject that connection without taking the daemon down.
                        tracing::warn!(%error, "local API peer credential check failed");
                        continue;
                    }
                };
                if peer_uid != expected_uid {
                    continue;
                }
                let service = service.clone();
                connections.spawn(async move {
                    if let Err(error) = handle_connection(stream, service).await {
                        tracing::warn!(%error, "local API connection closed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(%error, "local API connection task failed");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(cleanup);
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    service: HubService,
) -> Result<(), LocalServerError> {
    let mut framed = Framed::new(stream, codec());
    while let Some(frame) = framed.next().await {
        let frame = frame.map_err(|_| LocalServerError::MalformedFrame)?;
        let LocalFrame::Request { id, request } = serde_json::from_slice::<LocalFrame>(&frame)
            .map_err(|_| LocalServerError::MalformedFrame)?
        else {
            let _ = framed
                .send(
                    serde_json::to_vec(&LocalFrame::Error {
                        id: 0,
                        message: "client must send a request frame".into(),
                    })
                    .map_err(|_| LocalServerError::MalformedFrame)?
                    .as_slice(),
                )
                .await;
            continue;
        };
        if matches!(request, LocalRequest::Watch { .. }) {
            return watch_connection(id, request, &mut framed, service).await;
        }
        let response = match service.handle(request).await {
            Ok(response) => LocalFrame::Response { id, response },
            Err(error) => LocalFrame::Error {
                id,
                message: safe_error(&error),
            },
        };
        let bytes = serde_json::to_vec(&response).map_err(|_| LocalServerError::MalformedFrame)?;
        if framed.send(bytes.as_slice()).await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}

async fn watch_connection(
    id: u64,
    request: LocalRequest,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    service: HubService,
) -> Result<(), LocalServerError> {
    let LocalRequest::Watch { scope, since_seq } = request else {
        return Err(LocalServerError::MalformedFrame);
    };
    let ack = LocalFrame::Response {
        id,
        response: LocalResponseBody::Events(Vec::new()),
    };
    framed
        .send(
            serde_json::to_vec(&ack)
                .map_err(|_| LocalServerError::MalformedFrame)?
                .as_slice(),
        )
        .await
        .map_err(|_| LocalServerError::MalformedFrame)?;

    let (events, mut receiver) = mpsc::channel(64);
    let watcher = tokio::spawn(async move {
        let _ = service
            .watch(scope, since_seq, |event| events.try_send(event).is_ok())
            .await;
    });
    while let Some(event) = receiver.recv().await {
        let frame = LocalFrame::Event { id, event };
        let bytes = serde_json::to_vec(&frame).map_err(|_| LocalServerError::MalformedFrame)?;
        if framed.send(bytes.as_slice()).await.is_err() {
            break;
        }
    }
    watcher.abort();
    Ok(())
}

fn safe_error(error: &ServiceError) -> String {
    match error {
        ServiceError::Unsupported(message) => format!("unsupported: {message}"),
        ServiceError::Invalid(message) => format!("invalid request: {message}"),
        ServiceError::NotFound(message) => format!("not found: {message}"),
        ServiceError::Journal(error) => error.to_string(),
        ServiceError::Config(error) => error.to_string(),
        ServiceError::Git(error) => error.to_string(),
        ServiceError::NodePlane(error) => error.to_string(),
        ServiceError::Random => "secure random generation failed".into(),
        ServiceError::Blob(_)
        | ServiceError::Preparation(_)
        | ServiceError::Runner(_)
        | ServiceError::Worker
        | ServiceError::Internal => "internal service error".into(),
    }
}

fn prepare_socket_path(path: &Path) -> Result<(), LocalServerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(LocalServerError::UnsafeSocketPath);
            }
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => return Err(LocalServerError::AlreadyRunning),
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(path)?;
                }
                Err(error) => return Err(LocalServerError::Io(error)),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(LocalServerError::Io(error)),
    }
    Ok(())
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Rejects a watch scope that cannot identify any durable owner.
pub fn validate_watch_scope(scope: &WatchScope) -> Result<(), LocalServerError> {
    match scope {
        WatchScope::All | WatchScope::Dispatch(_) | WatchScope::Subtask(_) => Ok(()),
    }
}
