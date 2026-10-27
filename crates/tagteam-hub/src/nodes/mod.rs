//! Tonic node control plane, durable node authentication, and blob transport.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use data_encoding::BASE32_NOPAD;
use futures_util::Stream;
use sha2::{Digest, Sha256};
use tagteam_core::{
    Arch, Bytes, CachedRepo, Capacity, GitOid, Node, NodeId, NodePolicy, NodeState, Os,
    OwnerPresence, Platform, Thermal, Timestamp, Toolchain,
};
use tagteam_pki::{CertificateFingerprint, HubCa, Invite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Request, Response, Status,
    transport::{Certificate, Identity, Server, ServerTlsConfig},
};

use crate::{
    blobs::BlobStore,
    journal::{Command as JournalCommand, Journal, JournalError},
};

pub mod proto {
    pub use tagteam_proto::v1::*;
}

use proto::{
    BlobRef, Chunk, HubMessage, NodeMessage, chunk, hub_message,
    hub_server::{Hub, HubServer},
    node_message,
};

/// Maximum accepted content-addressed blob size (64 MiB).
pub const MAX_BLOB_SIZE: u64 = tagteam_core::MAX_BLOB_BYTES;
/// Maximum data bytes in a single blob stream chunk (1 MiB).
pub const MAX_CHUNK_SIZE: usize = 1024 * 1024;
const HEARTBEAT_MS: u32 = 15_000;

type HubStream = Pin<Box<dyn Stream<Item = Result<HubMessage, Status>> + Send + 'static>>;

#[derive(Clone)]
/// Tonic node plane backed by the hub journal and content-addressed blob store.
pub struct NodePlane {
    inner: Arc<Inner>,
}

struct Inner {
    journal: Mutex<Journal>,
    blob_root: PathBuf,
    ca: Arc<HubCa>,
    sessions: Mutex<HashMap<NodeId, SessionHandle>>,
    pending: Mutex<HashMap<String, oneshot::Sender<proto::CommandResult>>>,
    cancelled: Mutex<HashSet<String>>,
}

#[derive(Clone)]
struct SessionHandle {
    id: String,
    cancel: watch::Sender<bool>,
    outbound: mpsc::Sender<Result<HubMessage, Status>>,
}

impl NodePlane {
    /// Opens the durable journal and blob root for this node plane.
    pub async fn new(
        journal_path: impl AsRef<Path>,
        blob_root: impl AsRef<Path>,
        ca: HubCa,
    ) -> Result<Self, NodePlaneError> {
        Self::with_ca(journal_path, blob_root, Arc::new(ca)).await
    }

    /// Opens the node plane with an already-loaded hub CA shared by the local API.
    pub async fn with_ca(
        journal_path: impl AsRef<Path>,
        blob_root: impl AsRef<Path>,
        ca: Arc<HubCa>,
    ) -> Result<Self, NodePlaneError> {
        let journal_path = journal_path.as_ref().to_path_buf();
        let blob_root = blob_root.as_ref().to_path_buf();
        let blob_root_for_open = blob_root.clone();
        let journal = tokio::task::spawn_blocking(move || {
            let journal = Journal::open(journal_path)?;
            BlobStore::open(&blob_root_for_open)?;
            Ok::<_, NodePlaneError>(journal)
        })
        .await??;
        Ok(Self {
            inner: Arc::new(Inner {
                journal: Mutex::new(journal),
                blob_root,
                ca,
                sessions: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                cancelled: Mutex::new(HashSet::new()),
            }),
        })
    }

    /// Closes a stale session so a wedged node must reconnect and re-lease its
    /// assignments. Returns whether a session was present.
    pub fn disconnect(&self, node_id: NodeId) -> bool {
        let Ok(mut sessions) = self.inner.sessions.lock() else {
            return false;
        };
        let Some(session) = sessions.remove(&node_id) else {
            return false;
        };
        session.cancel.send_replace(true);
        true
    }

    /// Returns whether a node currently has a live session.
    pub fn is_online(&self, node_id: NodeId) -> bool {
        self.inner
            .sessions
            .lock()
            .map(|sessions| sessions.contains_key(&node_id))
            .unwrap_or(false)
    }

    /// Sends one durable command to the node and waits for its first result.
    ///
    /// The command is journaled before it leaves the hub. If the node reconnects
    /// with a stored acknowledgement, the persisted result is returned instead of
    /// running the operation again.
    pub async fn execute_command(
        &self,
        node_id: NodeId,
        command: proto::Command,
        timeout: Duration,
    ) -> Result<proto::CommandResult, NodePlaneError> {
        let command_id = command.command_id.clone();
        if command_id.is_empty() {
            return Err(NodePlaneError::Protocol(
                "command_id must not be empty".into(),
            ));
        }
        if let Some(result) = self.load_stored_result(&command_id).await? {
            return Ok(result);
        }
        let subtask_id = tagteam_core::SubtaskId::from_str(&command.subtask_id)
            .map_err(|_| NodePlaneError::Protocol("command subtask_id is invalid".into()))?;
        let journal_command = JournalCommand {
            id: command_id.clone(),
            node_id,
            subtask_id,
            operation: json_command(&command)?,
            state: "pending".into(),
            result: None,
            sent_at: Some(Timestamp::now()),
            done_at: None,
        };
        let journal = Arc::clone(&self.inner);
        let stored_id = command_id.clone();
        let cancelled = tokio::task::spawn_blocking(move || {
            // Serialize journal insertion with cancellation intent so a cancel
            // cannot land after this check but before the command row exists.
            let mut cancelled = journal
                .cancelled
                .lock()
                .map_err(|_| NodePlaneError::Internal)?;
            if cancelled.remove(&stored_id) {
                return Ok::<_, NodePlaneError>(true);
            }
            let mut store = journal
                .journal
                .lock()
                .map_err(|_| NodePlaneError::Protocol("journal lock is unavailable".into()))?;
            store
                .store_command(&journal_command)
                .map_err(NodePlaneError::Journal)?;
            Ok(false)
        })
        .await
        .map_err(|_| NodePlaneError::Internal)??;
        if cancelled {
            return Ok(cancelled_command_result(&command_id));
        }

        let (completion, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .map_err(|_| NodePlaneError::Internal)?
            .insert(command_id.clone(), completion);
        let outbound = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .map_err(|_| NodePlaneError::Internal)?;
            sessions
                .get(&node_id)
                .map(|session| session.outbound.clone())
                .ok_or(NodePlaneError::Offline)?
        };
        if outbound
            .send(Ok(HubMessage {
                msg: Some(hub_message::Msg::Command(command)),
            }))
            .await
            .is_err()
        {
            self.inner
                .pending
                .lock()
                .map_err(|_| NodePlaneError::Internal)?
                .remove(&command_id);
            return Err(NodePlaneError::Offline);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(NodePlaneError::Offline),
            Err(_) => {
                self.inner
                    .pending
                    .lock()
                    .map_err(|_| NodePlaneError::Internal)?
                    .remove(&command_id);
                Err(NodePlaneError::Timeout)
            }
        }
    }

    /// Asks the node to stop one in-flight command. Cancellation is best effort:
    /// the durable command journal still owns replay and idempotency.
    pub async fn cancel_command(
        &self,
        node_id: NodeId,
        command_id: &str,
        reason: &str,
    ) -> Result<(), NodePlaneError> {
        if command_id.is_empty() {
            return Err(NodePlaneError::Protocol(
                "command_id must not be empty".into(),
            ));
        }
        let journal = Arc::clone(&self.inner);
        let stored_id = command_id.to_owned();
        let outcome = tokio::task::spawn_blocking(move || {
            // This lock pairs with execute_command's journal insertion. A
            // command that has not reached its durable row is cancelled when
            // its future insertion observes this intent.
            let mut cancelled = journal
                .cancelled
                .lock()
                .map_err(|_| NodePlaneError::Internal)?;
            cancelled.insert(stored_id.clone());
            drop(cancelled);
            let mut store = journal
                .journal
                .lock()
                .map_err(|_| NodePlaneError::Internal)?;
            store
                .cancel_command(&stored_id)
                .map_err(NodePlaneError::Journal)
        })
        .await
        .map_err(|_| NodePlaneError::Internal)?;
        let outbound = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .map_err(|_| NodePlaneError::Internal)?;
            sessions
                .get(&node_id)
                .map(|session| session.outbound.clone())
                .ok_or(NodePlaneError::Offline)?
        };
        let sent = outbound
            .send(Ok(HubMessage {
                msg: Some(hub_message::Msg::Cancel(proto::Cancel {
                    command_id: command_id.to_owned(),
                    reason: reason.chars().take(512).collect(),
                })),
            }))
            .await
            .map_err(|_| NodePlaneError::Offline);
        match outcome {
            Ok(()) => sent,
            Err(NodePlaneError::Journal(JournalError::NotFound(_))) => sent,
            Err(error) => Err(error),
        }
    }

    async fn load_stored_result(
        &self,
        command_id: &str,
    ) -> Result<Option<proto::CommandResult>, NodePlaneError> {
        let journal = Arc::clone(&self.inner);
        let command_id = command_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let journal = journal
                .journal
                .lock()
                .map_err(|_| NodePlaneError::Internal)?;
            let stored = journal
                .load_command(&command_id)
                .map_err(NodePlaneError::Journal)?;
            stored
                .and_then(|command| command.result)
                .map(|value| result_from_json(&value))
                .transpose()
        })
        .await
        .map_err(|_| NodePlaneError::Internal)?
    }

    /// Serves the generated Hub service over mutual TLS until shutdown resolves.
    pub async fn serve<F>(
        &self,
        listener: tokio::net::TcpListener,
        shutdown: F,
    ) -> Result<(), NodePlaneError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let issued = self.inner.ca.issue_server_certificate()?;
        let identity = Identity::from_pem(
            format!(
                "{}{}",
                certificate_pem(&issued.certificate_der),
                certificate_pem(self.inner.ca.certificate_der()),
            ),
            private_key_pem(&issued.private_key_der),
        );
        let tls = ServerTlsConfig::new()
            .identity(identity)
            .client_ca_root(Certificate::from_pem(certificate_pem(
                self.inner.ca.certificate_der(),
            )))
            .client_auth_optional(true);
        let service = NodeService {
            inner: Arc::clone(&self.inner),
        };
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        Server::builder()
            .tls_config(tls)?
            .add_service(HubServer::new(service))
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await?;
        Ok(())
    }
}

use std::future::Future;

/// Construction and server failures from the node plane.
#[derive(Debug, thiserror::Error)]
pub enum NodePlaneError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Blob(#[from] crate::blobs::BlobError),
    #[error(transparent)]
    Pki(#[from] tagteam_pki::PkiError),
    #[error(transparent)]
    Transport(#[from] tonic::transport::Error),
    #[error("blocking worker stopped: {0}")]
    Blocking(#[from] tokio::task::JoinError),
    #[error("node is not connected")]
    Offline,
    #[error("node command timed out")]
    Timeout,
    #[error("node protocol error: {0}")]
    Protocol(String),
    #[error("internal node-plane error")]
    Internal,
}

#[derive(Clone)]
struct NodeService {
    inner: Arc<Inner>,
}

#[tonic::async_trait]
impl Hub for NodeService {
    type SessionStream = HubStream;
    type GetBlobStream = Pin<Box<dyn Stream<Item = Result<Chunk, Status>> + Send + 'static>>;

    async fn enroll(
        &self,
        request: Request<proto::EnrollRequest>,
    ) -> Result<Response<proto::EnrollReply>, Status> {
        let request = request.into_inner();
        let invite = Invite::decode(&request.code)
            .map_err(|_| Status::invalid_argument("enrollment invite is malformed"))?;
        if invite.hub_fingerprint != self.inner.ca.fingerprint() {
            return Err(Status::permission_denied("invite belongs to another hub"));
        }
        if request.name.trim().is_empty() {
            return Err(Status::invalid_argument("node name must not be empty"));
        }
        let platform = request
            .platform
            .ok_or_else(|| Status::invalid_argument("platform is required"))?;
        let platform = platform_from_proto(platform)?;
        let cert_der = self
            .inner
            .ca
            .sign_node_csr(&request.csr_der)
            .map_err(|_| Status::invalid_argument("CSR is invalid"))?;
        let id = NodeId::new();
        let cert_sha256 = *CertificateFingerprint::from_certificate(&cert_der).as_bytes();
        let mut node = Node {
            id,
            name: request.name,
            platform,
            state: NodeState::Enrolled,
            trust: tagteam_core::Trust::Own,
            cert_sha256,
            capacity: Capacity::default(),
            toolchains: Vec::new(),
            repo_heads: Vec::new(),
            policy: NodePolicy::default(),
            presence: OwnerPresence::Away {
                idle_for: Duration::ZERO,
            },
            last_seen: Timestamp::default(),
        };
        let code = BASE32_NOPAD.encode(invite.code_bytes());
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let now = Timestamp::now();
            node.last_seen = now;
            let mut journal = inner
                .journal
                .lock()
                .map_err(|_| Status::internal("journal lock is unavailable"))?;
            journal
                .enroll_node(&code, now, &node)
                .map_err(journal_status)
        })
        .await
        .map_err(|_| Status::internal("enrollment worker stopped"))??;
        Ok(Response::new(proto::EnrollReply {
            cert_der,
            node_id: id.to_string(),
        }))
    }

    async fn session(
        &self,
        request: Request<tonic::Streaming<NodeMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let cert = peer_certificate(&request)?;
        let node = self.authorize_by_cert(&cert).await?;
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("session must begin with Hello"))?;
        let Some(node_message::Msg::Hello(hello)) = first.msg else {
            return Err(Status::invalid_argument("session must begin with Hello"));
        };
        let hello_id = NodeId::from_str(&hello.node_id)
            .map_err(|_| Status::invalid_argument("Hello contains an invalid node ID"))?;
        if hello_id != node.id {
            return Err(Status::unauthenticated(
                "Hello node ID does not match client certificate",
            ));
        }
        for ack in hello.pending {
            let Some(result) = ack.result else {
                continue;
            };
            record_command_result(&self.inner, result).await?;
        }
        let id = node.id;
        let session_id = tagteam_core::MainSessionId::new().to_string();
        let (sender, receiver) = mpsc::channel(16);
        let (cancel, cancellation) = watch::channel(false);
        sender
            .send(Ok(HubMessage {
                msg: Some(hub_message::Msg::Welcome(proto::Welcome {
                    session_id: session_id.clone(),
                    heartbeat_ms: HEARTBEAT_MS,
                    node_state: node_state_to_proto(node.state) as i32,
                })),
            }))
            .await
            .map_err(|_| Status::internal("session response stream closed"))?;
        let previous = self
            .inner
            .sessions
            .lock()
            .map_err(|_| Status::internal("session registry is unavailable"))?
            .insert(
                id,
                SessionHandle {
                    id: session_id.clone(),
                    cancel,
                    outbound: sender.clone(),
                },
            );
        if let Some(previous) = previous {
            previous.cancel.send_replace(true);
        }
        // Replay after returning the response stream. Sending an unbounded set of
        // journaled commands through the bounded channel before the node can consume
        // the response would deadlock once the channel fills.
        let replay_inner = Arc::clone(&self.inner);
        let replay_sender = sender.clone();
        tokio::spawn(async move {
            let pending = match load_pending_commands(&replay_inner, id).await {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::warn!(%error, %id, "could not load pending node commands");
                    return;
                }
            };
            for command in pending {
                if replay_sender
                    .send(Ok(HubMessage {
                        msg: Some(hub_message::Msg::Command(command)),
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut cancellation = cancellation;
            loop {
                let next = tokio::select! {
                    _ = cancellation.changed() => break,
                    message = stream.message() => message,
                };
                let Ok(Some(message)) = next else {
                    break;
                };
                match message.msg {
                    Some(node_message::Msg::Heartbeat(heartbeat)) => {
                        if let Err(error) = update_heartbeat(&inner, id, heartbeat).await {
                            let _ = sender.send(Err(error)).await;
                            break;
                        }
                    }
                    Some(node_message::Msg::Result(result)) => {
                        if let Err(error) = record_command_result(&inner, result).await {
                            let _ = sender.send(Err(error)).await;
                            break;
                        }
                    }
                    Some(node_message::Msg::Hello(_)) => {}
                    Some(node_message::Msg::Output(_)) | None => {}
                }
            }
            if let Ok(mut sessions) = inner.sessions.lock()
                && sessions
                    .get(&id)
                    .is_some_and(|handle| handle.id == session_id)
            {
                sessions.remove(&id);
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn get_blob(
        &self,
        request: Request<BlobRef>,
    ) -> Result<Response<Self::GetBlobStream>, Status> {
        let cert = peer_certificate(&request)?;
        let reference = request.into_inner();
        self.authorize_by_cert(&cert).await?;
        let reference = blob_ref_from_proto(reference)?;
        if reference.size > MAX_BLOB_SIZE {
            return Err(Status::resource_exhausted("blob exceeds 64 MiB limit"));
        }
        let root = self.inner.blob_root.clone();
        let read_reference = reference.clone();
        let data = tokio::task::spawn_blocking(move || {
            BlobStore::open(root)
                .and_then(|store| store.read(&read_reference))
                .map_err(blob_status)
        })
        .await
        .map_err(|_| Status::internal("blob worker stopped"))??;
        let mut chunks = Vec::with_capacity(data.len().div_ceil(MAX_CHUNK_SIZE) + 1);
        chunks.push(Ok(Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: reference.sha256.to_vec(),
                size: reference.size,
            })),
        }));
        chunks.extend(data.chunks(MAX_CHUNK_SIZE).map(|part| {
            Ok(Chunk {
                body: Some(chunk::Body::Data(part.to_vec())),
            })
        }));
        Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
    }

    async fn put_blob(
        &self,
        request: Request<tonic::Streaming<Chunk>>,
    ) -> Result<Response<BlobRef>, Status> {
        let cert = peer_certificate(&request)?;
        self.authorize_by_cert(&cert).await?;
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("blob stream is missing its header"))?;
        let Some(chunk::Body::Header(header)) = first.body else {
            return Err(Status::invalid_argument(
                "first blob chunk must be a header",
            ));
        };
        if header.sha256.len() != 32 {
            return Err(Status::invalid_argument(
                "blob header digest must be 32 bytes",
            ));
        }
        if header.size > MAX_BLOB_SIZE {
            return Err(Status::resource_exhausted("blob exceeds 64 MiB limit"));
        }
        let mut bytes = Vec::with_capacity(header.size as usize);
        while let Some(next) = stream.message().await? {
            match next.body {
                Some(chunk::Body::Data(data)) => {
                    if data.len() > MAX_CHUNK_SIZE {
                        return Err(Status::resource_exhausted("blob chunk exceeds 1 MiB limit"));
                    }
                    if data.is_empty() {
                        return Err(Status::invalid_argument(
                            "blob data chunks must not be empty",
                        ));
                    }
                    if bytes.len().saturating_add(data.len()) as u64 > header.size
                        || bytes.len().saturating_add(data.len()) as u64 > MAX_BLOB_SIZE
                    {
                        return Err(Status::invalid_argument("blob data exceeds declared size"));
                    }
                    bytes.extend_from_slice(&data);
                }
                Some(chunk::Body::Header(_)) => {
                    return Err(Status::invalid_argument(
                        "blob stream has more than one header",
                    ));
                }
                None => return Err(Status::invalid_argument("blob chunk is empty")),
            }
        }
        if bytes.len() as u64 != header.size {
            return Err(Status::invalid_argument(
                "blob data does not match declared size",
            ));
        }
        let claimed_digest = header.sha256;
        let computed_digest: [u8; 32] = Sha256::digest(&bytes).into();
        if computed_digest.as_slice() != claimed_digest.as_slice() {
            return Err(Status::invalid_argument(
                "blob content does not match header digest",
            ));
        }
        let declared_size = header.size;
        if bytes.len() as u64 != declared_size {
            return Err(Status::invalid_argument(
                "blob content does not match header size",
            ));
        }
        let root = self.inner.blob_root.clone();
        let reference = tokio::task::spawn_blocking(move || {
            let store = BlobStore::open(root).map_err(blob_status)?;
            let reference = store.put(&bytes).map_err(blob_status)?;
            if reference.sha256 != computed_digest || reference.size != declared_size {
                return Err(Status::invalid_argument(
                    "blob content does not match header",
                ));
            }
            Ok::<_, Status>(reference)
        })
        .await
        .map_err(|_| Status::internal("blob worker stopped"))??;
        Ok(Response::new(blob_ref_to_proto(reference)))
    }
}

impl NodeService {
    async fn authorize_by_cert(&self, certificate: &[u8]) -> Result<Node, Status> {
        let fingerprint = *CertificateFingerprint::from_certificate(certificate).as_bytes();
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let journal = inner
                .journal
                .lock()
                .map_err(|_| Status::internal("journal lock is unavailable"))?;
            let node = journal
                .load_node_by_certificate_sha256(&fingerprint)
                .map_err(journal_status)?
                .ok_or_else(|| Status::unauthenticated("node certificate is not authorized"))?;
            if node.state == NodeState::Removed {
                return Err(Status::unauthenticated(
                    "node certificate is not authorized",
                ));
            }
            Ok(node)
        })
        .await
        .map_err(|_| Status::internal("authorization worker stopped"))?
    }
}

fn peer_certificate<T>(request: &Request<T>) -> Result<Vec<u8>, Status> {
    let certs = request
        .peer_certs()
        .ok_or_else(|| Status::unauthenticated("client certificate is required"))?;
    certs
        .first()
        .map(|cert| cert.as_ref().to_vec())
        .ok_or_else(|| Status::unauthenticated("client certificate is required"))
}

async fn update_heartbeat(
    inner: &Arc<Inner>,
    id: NodeId,
    heartbeat: proto::Heartbeat,
) -> Result<(), Status> {
    let capacity = capacity_from_proto(
        heartbeat
            .capacity
            .ok_or_else(|| Status::invalid_argument("heartbeat capacity is required"))?,
    )?;
    let presence = match proto::OwnerPresence::try_from(heartbeat.presence)
        .map_err(|_| Status::invalid_argument("heartbeat presence is invalid"))?
    {
        proto::OwnerPresence::Away => OwnerPresence::Away {
            idle_for: Duration::from_millis(heartbeat.idle_for_ms),
        },
        proto::OwnerPresence::Present => OwnerPresence::Present,
        proto::OwnerPresence::Unspecified => {
            return Err(Status::invalid_argument("heartbeat presence is required"));
        }
    };
    let toolchains = heartbeat
        .toolchains
        .into_iter()
        .map(toolchain_from_proto)
        .collect::<Result<Vec<_>, _>>()?;
    let repo_heads = heartbeat
        .repos
        .into_iter()
        .map(repo_from_proto)
        .collect::<Result<Vec<_>, _>>()?;
    let inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        let last_seen = Timestamp::now();
        let mut journal = inner
            .journal
            .lock()
            .map_err(|_| Status::internal("journal lock is unavailable"))?;
        journal
            .update_node_heartbeat(id, &capacity, &toolchains, &repo_heads, presence, last_seen)
            .map_err(journal_status)
    })
    .await
    .map_err(|_| Status::internal("heartbeat worker stopped"))?
}

fn platform_from_proto(value: proto::Platform) -> Result<Platform, Status> {
    let os = match proto::Os::try_from(value.os)
        .map_err(|_| Status::invalid_argument("platform OS is invalid"))?
    {
        proto::Os::Macos => Os::MacOs,
        proto::Os::Linux => Os::Linux,
        proto::Os::Unspecified => return Err(Status::invalid_argument("platform OS is required")),
    };
    let arch = match proto::Arch::try_from(value.arch)
        .map_err(|_| Status::invalid_argument("platform architecture is invalid"))?
    {
        proto::Arch::Arm64 => Arch::Arm64,
        proto::Arch::X64 => Arch::X64,
        proto::Arch::Unspecified => {
            return Err(Status::invalid_argument(
                "platform architecture is required",
            ));
        }
    };
    Ok(Platform {
        os,
        arch,
        os_version: value.os_version,
    })
}

fn capacity_from_proto(value: proto::Capacity) -> Result<Capacity, Status> {
    if !value.cores_free.is_finite() || value.cores_free < 0.0 {
        return Err(Status::invalid_argument(
            "heartbeat free core count is invalid",
        ));
    }
    let thermal = match value.thermal.as_str() {
        "nominal" => Thermal::Nominal,
        "fair" => Thermal::Fair,
        "serious" => Thermal::Serious,
        "critical" => Thermal::Critical,
        _ => {
            return Err(Status::invalid_argument(
                "heartbeat thermal state is invalid",
            ));
        }
    };
    Ok(Capacity {
        cores_total: u16::try_from(value.cores_total)
            .map_err(|_| Status::invalid_argument("heartbeat core count is out of range"))?,
        cores_free: value.cores_free,
        memory_free: Bytes(value.memory_free),
        disk_free: Bytes(value.disk_free),
        running: u16::try_from(value.running)
            .map_err(|_| Status::invalid_argument("heartbeat running count is out of range"))?,
        on_battery: value.on_battery,
        thermal,
    })
}

fn toolchain_from_proto(value: proto::Toolchain) -> Result<Toolchain, Status> {
    let version = semver::Version::parse(&value.version)
        .map_err(|_| Status::invalid_argument("toolchain version is invalid"))?;
    if value.name.is_empty() || value.path.is_empty() {
        return Err(Status::invalid_argument(
            "toolchain name and path are required",
        ));
    }
    Ok(Toolchain {
        name: value.name,
        version,
        path: value.path.into(),
    })
}

fn repo_from_proto(value: proto::CachedRepo) -> Result<CachedRepo, Status> {
    if value.repo_id.is_empty() {
        return Err(Status::invalid_argument("cached repository ID is required"));
    }
    Ok(CachedRepo {
        repo: tagteam_core::RepoId(GitOid(value.repo_id)),
        heads: value.heads.into_iter().map(GitOid).collect(),
    })
}

fn node_state_to_proto(value: NodeState) -> proto::NodeState {
    match value {
        NodeState::Enrolled => proto::NodeState::Enrolled,
        NodeState::Active => proto::NodeState::Active,
        NodeState::OwnerBusy => proto::NodeState::OwnerBusy,
        NodeState::Paused => proto::NodeState::Paused,
        NodeState::Draining => proto::NodeState::Draining,
        NodeState::Lost => proto::NodeState::Lost,
        NodeState::Removed => proto::NodeState::Removed,
    }
}

fn blob_ref_from_proto(value: BlobRef) -> Result<tagteam_core::BlobRef, Status> {
    let sha256: [u8; 32] = value
        .sha256
        .try_into()
        .map_err(|_| Status::invalid_argument("blob digest must be 32 bytes"))?;
    Ok(tagteam_core::BlobRef {
        sha256,
        size: value.size,
    })
}

fn blob_ref_to_proto(value: tagteam_core::BlobRef) -> BlobRef {
    BlobRef {
        sha256: value.sha256.to_vec(),
        size: value.size,
    }
}

fn journal_status(error: JournalError) -> Status {
    match error {
        JournalError::InvalidInviteCode
        | JournalError::InviteExpired
        | JournalError::InviteAlreadyUsed => Status::permission_denied(error.to_string()),
        JournalError::NotFound(_) => Status::not_found(error.to_string()),
        JournalError::NodeNameAlreadyExists => Status::already_exists(error.to_string()),
        _ => Status::internal("journal operation failed"),
    }
}

fn blob_status(error: crate::blobs::BlobError) -> Status {
    match error {
        crate::blobs::BlobError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Status::not_found("blob was not found")
        }
        crate::blobs::BlobError::DigestMismatch | crate::blobs::BlobError::SizeMismatch => {
            Status::data_loss("stored blob failed integrity verification")
        }
        _ => Status::internal("blob storage operation failed"),
    }
}

fn certificate_pem(der: &[u8]) -> String {
    pem("CERTIFICATE", der)
}

fn private_key_pem(der: &[u8]) -> String {
    pem("PRIVATE KEY", der)
}

fn pem(kind: &str, der: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {kind}-----\n");
    for start in (0..encoded.len()).step_by(64) {
        let end = (start + 64).min(encoded.len());
        pem.push_str(&encoded[start..end]);
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {kind}-----\n"));
    pem
}

async fn record_command_result(
    inner: &Arc<Inner>,
    result: proto::CommandResult,
) -> Result<(), Status> {
    let command_id = result.command_id.clone();
    let stored_id = command_id.clone();
    let encoded =
        result_to_json(&result).map_err(|_| Status::internal("command result encoding failed"))?;
    let worker_inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        let mut journal = worker_inner
            .journal
            .lock()
            .map_err(|_| Status::internal("journal lock is unavailable"))?;
        journal
            .store_command_result(&stored_id, &encoded, Timestamp::now())
            .map_err(journal_status)
    })
    .await
    .map_err(|_| Status::internal("command result worker stopped"))??;
    if let Ok(mut pending) = inner.pending.lock()
        && let Some(completion) = pending.remove(&command_id)
    {
        let _ = completion.send(result);
    }
    Ok(())
}

fn cancelled_command_result(command_id: &str) -> proto::CommandResult {
    proto::CommandResult {
        command_id: command_id.to_owned(),
        result: Some(proto::command_result::Result::Error(proto::Error {
            code: proto::ErrorCode::Cancelled as i32,
            message: "command was cancelled before dispatch".into(),
            retryable: false,
        })),
    }
}

fn json_command(command: &proto::Command) -> Result<serde_json::Value, NodePlaneError> {
    use base64::Engine as _;
    use prost::Message as _;
    let bytes = command.encode_to_vec();
    Ok(serde_json::json!({
        "proto_b64": base64::engine::general_purpose::STANDARD.encode(bytes)
    }))
}

fn result_to_json(result: &proto::CommandResult) -> Result<serde_json::Value, NodePlaneError> {
    use base64::Engine as _;
    use prost::Message as _;
    let bytes = result.encode_to_vec();
    Ok(serde_json::json!({
        "proto_b64": base64::engine::general_purpose::STANDARD.encode(bytes)
    }))
}

fn result_from_json(value: &serde_json::Value) -> Result<proto::CommandResult, NodePlaneError> {
    use base64::Engine as _;
    use prost::Message as _;
    let encoded = value
        .get("proto_b64")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NodePlaneError::Protocol("stored command result is malformed".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| NodePlaneError::Protocol("stored command result is not base64".into()))?;
    proto::CommandResult::decode(bytes.as_slice())
        .map_err(|_| NodePlaneError::Protocol("stored command result is invalid protobuf".into()))
}

async fn load_pending_commands(
    inner: &Arc<Inner>,
    node_id: NodeId,
) -> Result<Vec<proto::Command>, Status> {
    let worker_inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        let journal = worker_inner
            .journal
            .lock()
            .map_err(|_| Status::internal("journal lock is unavailable"))?;
        let commands = journal.pending_commands(node_id).map_err(journal_status)?;
        commands
            .into_iter()
            .map(|command| command_from_json(&command.operation))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Status::internal("stored command is malformed"))
    })
    .await
    .map_err(|_| Status::internal("pending command worker stopped"))?
}

fn command_from_json(value: &serde_json::Value) -> Result<proto::Command, NodePlaneError> {
    use base64::Engine as _;
    use prost::Message as _;
    let encoded = value
        .get("proto_b64")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NodePlaneError::Protocol("stored command is malformed".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| NodePlaneError::Protocol("stored command is not base64".into()))?;
    proto::Command::decode(bytes.as_slice())
        .map_err(|_| NodePlaneError::Protocol("stored command is invalid protobuf".into()))
}

#[cfg(test)]
mod tests;
