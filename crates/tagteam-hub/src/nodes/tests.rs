//! Loopback mTLS and transactional behavior tests for the node plane.

use std::{str::FromStr, time::Duration};

use data_encoding::BASE32_NOPAD;
use sha2::Digest;
use tagteam_core::{
    Arch, Capacity, Node, NodeId, NodePolicy, NodeState, Os, OwnerPresence, Platform, Thermal,
    Timestamp, Trust,
};
use tagteam_pki::{CertificateFingerprint, HubCa, Invite, NodeKeyRequest, SERVICE_DNS_NAME};
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tonic::{
    Code, Status,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity},
};

use super::{
    NodePlane, certificate_pem, chunk, json_command, policy_from_proto, private_key_pem,
    proto::{self, hub_client::HubClient},
};
use crate::journal::{Command as JournalCommand, Journal};

fn sample_node(id: NodeId, name: &str, certificate: [u8; 32]) -> Node {
    Node {
        id,
        name: name.to_owned(),
        platform: Platform {
            os: Os::Linux,
            arch: Arch::X64,
            os_version: "test".into(),
        },
        state: NodeState::Enrolled,
        trust: Trust::Own,
        cert_sha256: certificate,
        capacity: Capacity::default(),
        toolchains: Vec::new(),
        repo_heads: Vec::new(),
        policy: NodePolicy::default(),
        presence: OwnerPresence::Away {
            idle_for: Duration::ZERO,
        },
        last_seen: Timestamp(1),
    }
}

#[test]
fn enrollment_failure_rolls_back_invite_consumption_and_node_insert() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("journal.sqlite");
    let mut journal = Journal::open(&path).expect("journal");
    let node_id = NodeId::new();
    journal
        .create_node(&sample_node(node_id, "existing", [1; 32]))
        .expect("seed node");
    let code = BASE32_NOPAD.encode(&[9; 32]);
    journal
        .create_invite(&code, "invite label", Timestamp(i64::MAX))
        .expect("invite");

    let duplicate_id_with_distinct_node = sample_node(node_id, "new name", [2; 32]);
    let result = journal.enroll_node(&code, Timestamp(2), &duplicate_id_with_distinct_node);
    assert!(result.is_err());
    let invite = journal
        .load_invite(&code)
        .expect("load invite")
        .expect("invite exists");
    assert_eq!(invite.used_at, None);
    assert_eq!(invite.node_id, None);
    assert_eq!(
        journal
            .load_node(node_id)
            .expect("load node")
            .expect("node")
            .name,
        "existing"
    );
    let connection = rusqlite::Connection::open(path).expect("open journal for assertion");
    let new_name_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE name='new name')",
            [],
            |row| row.get(0),
        )
        .expect("check attempted node name");
    assert!(!new_name_exists);
}

#[test]
fn heartbeat_updates_live_fields_without_changing_state_trust_or_policy() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut journal = Journal::open(directory.path().join("journal.sqlite")).expect("journal");
    let id = NodeId::new();
    let mut node = sample_node(id, "heartbeat", [4; 32]);
    node.state = NodeState::Paused;
    node.trust = Trust::Friend;
    node.policy.max_subtasks = 7;
    journal.create_node(&node).expect("seed node");
    let capacity = Capacity {
        cores_total: 8,
        cores_free: 3.5,
        battery_known: true,
        local_minute_of_day: Some(22 * 60 + 15),
        thermal: Thermal::Fair,
        ..Capacity::default()
    };
    journal
        .update_node_heartbeat(
            id,
            &capacity,
            &[],
            &[],
            OwnerPresence::Present,
            Timestamp(42),
        )
        .expect("heartbeat");
    let saved = journal
        .load_node(id)
        .expect("load node")
        .expect("node exists");
    assert_eq!(saved.state, NodeState::Paused);
    assert_eq!(saved.trust, Trust::Friend);
    assert_eq!(saved.policy.max_subtasks, 7);
    assert_eq!(saved.capacity, capacity);
    assert_eq!(saved.presence, OwnerPresence::Present);
    assert_eq!(saved.last_seen, Timestamp(42));
}

async fn client(address: std::net::SocketAddr, ca_der: &[u8]) -> HubClient<Channel> {
    let channel = Endpoint::from_shared(format!("https://{address}"))
        .expect("endpoint")
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(certificate_pem(ca_der)))
                .domain_name(SERVICE_DNS_NAME),
        )
        .expect("TLS config")
        .connect()
        .await
        .expect("connect");
    HubClient::new(channel)
}

fn request_stream<T: Send + 'static>(
    items: Vec<Result<T, Status>>,
) -> impl futures_util::Stream<Item = T> + Send + 'static {
    tokio_stream::iter(
        items
            .into_iter()
            .map(|item| item.expect("test request item")),
    )
}

async fn start_server(
    directory: &tempfile::TempDir,
    ca: HubCa,
) -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let plane = NodePlane::new(
        directory.path().join("journal.sqlite"),
        directory.path().join("blobs"),
        ca,
    )
    .await
    .expect("node plane");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("listener address");
    let (shutdown, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let _ = plane
            .serve(listener, async move {
                let _ = receiver.await;
            })
            .await;
    });
    (address, shutdown)
}

#[tokio::test]
async fn loopback_mtls_enrolls_and_authenticates_blob_calls() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let ca = HubCa::generate().expect("CA");
    let ca_der = ca.certificate_der().to_vec();
    let ca_fingerprint = ca.fingerprint();
    let invite_code = [7; 32];
    let invite =
        Invite::new("https://127.0.0.1:9443", ca_fingerprint, invite_code).expect("invite");
    let expired_code = [8; 32];
    let expired_invite = Invite::new("https://127.0.0.1:9443", ca_fingerprint, expired_code)
        .expect("expired invite");
    let duplicate_name_code = [10; 32];
    let mut journal = Journal::open(directory.path().join("journal.sqlite")).expect("journal");
    journal
        .create_invite(
            &BASE32_NOPAD.encode(&invite_code),
            "display label",
            Timestamp(i64::MAX),
        )
        .expect("seed invite");
    journal
        .create_invite(&BASE32_NOPAD.encode(&expired_code), "expired", Timestamp(1))
        .expect("seed expired invite");
    journal
        .create_invite(
            &BASE32_NOPAD.encode(&duplicate_name_code),
            "duplicate name",
            Timestamp(i64::MAX),
        )
        .expect("seed duplicate name invite");
    drop(journal);
    let (address, shutdown) = start_server(&directory, ca).await;
    let mut enrollment_client = client(address, &ca_der).await;
    let key = NodeKeyRequest::generate("node-test").expect("node key");
    let expired = enrollment_client
        .enroll(proto::EnrollRequest {
            code: expired_invite.encode(),
            csr_der: key.csr_der.clone(),
            name: "expired node".into(),
            platform: Some(proto::Platform {
                os: proto::Os::Linux as i32,
                arch: proto::Arch::X64 as i32,
                os_version: "test".into(),
            }),
        })
        .await
        .expect_err("expired invite rejected");
    assert_eq!(expired.code(), Code::PermissionDenied);
    let foreign_ca = HubCa::generate().expect("foreign CA");
    let wrong_ca_invite = Invite::new("https://127.0.0.1:9443", foreign_ca.fingerprint(), [6; 32])
        .expect("foreign invite");
    let wrong_ca = enrollment_client
        .enroll(proto::EnrollRequest {
            code: wrong_ca_invite.encode(),
            csr_der: key.csr_der.clone(),
            name: "wrong CA node".into(),
            platform: Some(proto::Platform {
                os: proto::Os::Linux as i32,
                arch: proto::Arch::X64 as i32,
                os_version: "test".into(),
            }),
        })
        .await
        .expect_err("wrong CA invite rejected");
    assert_eq!(wrong_ca.code(), Code::PermissionDenied);
    let malformed_invite = enrollment_client
        .enroll(proto::EnrollRequest {
            code: "not-an-invite".into(),
            csr_der: key.csr_der.clone(),
            name: "malformed invite node".into(),
            platform: Some(proto::Platform {
                os: proto::Os::Linux as i32,
                arch: proto::Arch::X64 as i32,
                os_version: "test".into(),
            }),
        })
        .await
        .expect_err("malformed invite rejected");
    assert_eq!(malformed_invite.code(), Code::InvalidArgument);
    let malformed_csr = enrollment_client
        .enroll(proto::EnrollRequest {
            code: invite.encode(),
            csr_der: vec![0, 1, 2, 3],
            name: "malformed CSR node".into(),
            platform: Some(proto::Platform {
                os: proto::Os::Linux as i32,
                arch: proto::Arch::X64 as i32,
                os_version: "test".into(),
            }),
        })
        .await
        .expect_err("malformed CSR rejected");
    assert_eq!(malformed_csr.code(), Code::InvalidArgument);
    let enrolled = enrollment_client
        .enroll(proto::EnrollRequest {
            code: invite.encode(),
            csr_der: key.csr_der.clone(),
            name: "requested node name".into(),
            platform: Some(proto::Platform {
                os: proto::Os::Linux as i32,
                arch: proto::Arch::X64 as i32,
                os_version: "test".into(),
            }),
        })
        .await
        .expect("enroll")
        .into_inner();
    let id = NodeId::from_str(&enrolled.node_id).expect("node ID");
    let persisted = tokio::task::spawn_blocking({
        let path = directory.path().join("journal.sqlite");
        let fingerprint = *CertificateFingerprint::from_certificate(&enrolled.cert_der).as_bytes();
        move || {
            Journal::open(path)
                .expect("journal")
                .load_node_by_certificate_sha256(&fingerprint)
                .expect("certificate lookup")
                .expect("certificate fingerprint persisted")
        }
    })
    .await
    .expect("lookup worker");
    assert_eq!(persisted.id, id);
    assert_eq!(persisted.name, "requested node name");
    assert_eq!(persisted.platform.os, Os::Linux);
    assert_eq!(persisted.state, NodeState::Enrolled);
    assert_eq!(persisted.trust, Trust::Own);
    assert_eq!(
        persisted.cert_sha256,
        *CertificateFingerprint::from_certificate(&enrolled.cert_der).as_bytes()
    );

    let duplicate_name_key = NodeKeyRequest::generate("duplicate-name").expect("node key");
    let duplicate_name_invite = Invite::new(
        "https://127.0.0.1:9443",
        ca_fingerprint,
        duplicate_name_code,
    )
    .expect("duplicate name invite");
    let duplicate_name = enrollment_client
        .enroll(proto::EnrollRequest {
            code: duplicate_name_invite.encode(),
            csr_der: duplicate_name_key.csr_der,
            name: "requested node name".into(),
            platform: Some(proto::Platform {
                os: proto::Os::Linux as i32,
                arch: proto::Arch::X64 as i32,
                os_version: "test".into(),
            }),
        })
        .await
        .expect_err("duplicate node name rejected");
    assert_eq!(duplicate_name.code(), Code::AlreadyExists);

    let reused = enrollment_client
        .enroll(proto::EnrollRequest {
            code: invite.encode(),
            csr_der: key.csr_der.clone(),
            name: "reused invite".into(),
            platform: Some(proto::Platform {
                os: proto::Os::Linux as i32,
                arch: proto::Arch::X64 as i32,
                os_version: "test".into(),
            }),
        })
        .await
        .expect_err("used invite rejected");
    assert_eq!(reused.code(), Code::PermissionDenied);

    let authenticated = Endpoint::from_shared(format!("https://{address}"))
        .expect("endpoint")
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(certificate_pem(&ca_der)))
                .domain_name(SERVICE_DNS_NAME)
                .identity(Identity::from_pem(
                    certificate_pem(&enrolled.cert_der),
                    private_key_pem(&key.private_key_der),
                )),
        )
        .expect("client identity")
        .connect()
        .await
        .expect("authenticated connection");
    let mut hub = HubClient::new(authenticated.clone());
    let wrong_ca_key = NodeKeyRequest::generate("wrong-ca-client").expect("wrong CA key");
    let wrong_ca_certificate = foreign_ca
        .sign_node_csr(&wrong_ca_key.csr_der)
        .expect("wrong CA certificate");
    let wrong_ca_connection = Endpoint::from_shared(format!("https://{address}"))
        .expect("wrong CA endpoint")
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(certificate_pem(&ca_der)))
                .domain_name(SERVICE_DNS_NAME)
                .identity(Identity::from_pem(
                    certificate_pem(&wrong_ca_certificate),
                    private_key_pem(&wrong_ca_key.private_key_der),
                )),
        )
        .expect("wrong CA TLS config")
        .connect()
        .await;
    let wrong_ca_rejected = match wrong_ca_connection {
        Err(_) => true,
        Ok(channel) => HubClient::new(channel)
            .get_blob(proto::BlobRef {
                sha256: vec![0; 32],
                size: 0,
            })
            .await
            .is_err(),
    };
    assert!(
        wrong_ca_rejected,
        "a client certificate from another CA must fail TLS"
    );
    let denied = client(address, &ca_der)
        .await
        .get_blob(proto::BlobRef {
            sha256: vec![0; 32],
            size: 0,
        })
        .await;
    let denied = match denied {
        Err(error) => error,
        Ok(_) => panic!("unauthenticated call was accepted"),
    };
    assert_eq!(denied.code(), Code::Unauthenticated);
    let mut unauthenticated = client(address, &ca_der).await;
    let unauth_session = unauthenticated
        .session(request_stream(vec![Ok::<_, Status>(proto::NodeMessage {
            msg: Some(proto::node_message::Msg::Hello(proto::Hello {
                node_id: enrolled.node_id.clone(),
                version: "test".into(),
                workspaces: Vec::new(),
                pending: Vec::new(),
                policy: None,
            })),
        })]))
        .await;
    let unauth_session = match unauth_session {
        Err(error) => error,
        Ok(_) => panic!("unauthenticated session was accepted"),
    };
    assert_eq!(unauth_session.code(), Code::Unauthenticated);
    let unauth_put = unauthenticated
        .put_blob(request_stream(vec![Ok::<_, Status>(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: vec![0; 32],
                size: 0,
            })),
        })]))
        .await
        .expect_err("unauthenticated upload rejected");
    assert_eq!(unauth_put.code(), Code::Unauthenticated);

    let mismatched_hello = hub
        .session(request_stream(vec![Ok::<_, Status>(proto::NodeMessage {
            msg: Some(proto::node_message::Msg::Hello(proto::Hello {
                node_id: NodeId::new().to_string(),
                version: "test".into(),
                workspaces: Vec::new(),
                pending: Vec::new(),
                policy: None,
            })),
        })]))
        .await
        .expect_err("Hello ID must match the client certificate");
    assert_eq!(mismatched_hello.code(), Code::Unauthenticated);

    tokio::task::spawn_blocking({
        let path = directory.path().join("journal.sqlite");
        move || {
            Journal::open(path)
                .expect("journal")
                .approve_node(id)
                .expect("approve node")
        }
    })
    .await
    .expect("approval worker");

    let id_string = enrolled.node_id.clone();
    let (first_sender, first_messages) = tokio::sync::mpsc::channel(4);
    first_sender
        .send(proto::NodeMessage {
            msg: Some(proto::node_message::Msg::Hello(proto::Hello {
                node_id: id_string.clone(),
                version: "test".into(),
                workspaces: Vec::new(),
                pending: Vec::new(),
                policy: None,
            })),
        })
        .await
        .expect("send first Hello");
    let mut first_session = hub
        .session(tokio_stream::wrappers::ReceiverStream::new(first_messages))
        .await
        .expect("first session")
        .into_inner();
    let first_welcome = first_session
        .message()
        .await
        .expect("first welcome")
        .expect("welcome");
    assert!(matches!(
        first_welcome.msg,
        Some(proto::hub_message::Msg::Welcome(proto::Welcome {
            node_state: state,
            heartbeat_ms: 15_000,
            ref session_id,
        })) if state == proto::NodeState::Active as i32 && !session_id.is_empty()
    ));

    let replay_subtask = tagteam_core::SubtaskId::new();
    let replay_dispatch = tagteam_core::DispatchId::new();
    let replay_total = 24_u32;
    tokio::task::spawn_blocking({
        let path = directory.path().join("journal.sqlite");
        move || {
            let mut journal = Journal::open(path).expect("journal");
            let dispatch = tagteam_core::Dispatch {
                id: replay_dispatch,
                main_session: tagteam_core::MainSessionId::new(),
                claude_session: None,
                repo: std::path::PathBuf::from("/workspace/repo"),
                created: Timestamp(1),
            };
            journal.create_dispatch(&dispatch).expect("store dispatch");
            journal
                .create_subtask(&tagteam_core::Subtask {
                    id: replay_subtask,
                    dispatch: replay_dispatch,
                    spec: tagteam_core::SubtaskSpec {
                        title: "replay".into(),
                        prompt: "replay pending commands".into(),
                        done_when: Vec::new(),
                        repo: std::path::PathBuf::from("/workspace/repo"),
                        base_ref: "main".into(),
                        include_uncommitted: false,
                        needs: tagteam_core::Needs::default(),
                        limits: tagteam_core::Limits::default(),
                        egress: Vec::new(),
                        node_hint: None,
                    },
                    state: tagteam_core::SubtaskState::Queued,
                    node: None,
                    attempt: 0,
                    base: tagteam_core::GitOid("0123456789012345678901234567890123456789".into()),
                    repo_id: tagteam_core::RepoId(tagteam_core::GitOid(
                        "1123456789012345678901234567890123456789".into(),
                    )),
                    branch: "tagteam/replay".into(),
                    runner_session: None,
                    created: Timestamp(2),
                    started: None,
                    finished: None,
                    usage: tagteam_core::Usage::default(),
                })
                .expect("store subtask");
            for index in 0..replay_total {
                let command = proto::Command {
                    command_id: format!("replay-{index:02}"),
                    subtask_id: replay_subtask.to_string(),
                    workspace: "workspace".into(),
                    op: Some(proto::command::Op::Cleanup(proto::Cleanup {
                        keep_cache: false,
                    })),
                };
                journal
                    .store_command(&JournalCommand {
                        id: command.command_id.clone(),
                        node_id: id,
                        subtask_id: replay_subtask,
                        operation: json_command(&command).expect("encode command"),
                        state: "pending".into(),
                        result: None,
                        sent_at: Some(Timestamp(i64::from(index))),
                        done_at: None,
                    })
                    .expect("store pending command");
            }
        }
    })
    .await
    .expect("pending command seed");
    let (second_sender, second_messages) = tokio::sync::mpsc::channel(4);
    second_sender
        .send(proto::NodeMessage {
            msg: Some(proto::node_message::Msg::Hello(proto::Hello {
                node_id: id_string,
                version: "test".into(),
                workspaces: Vec::new(),
                pending: Vec::new(),
                policy: None,
            })),
        })
        .await
        .expect("send replacement Hello");
    let mut second_session = tokio::time::timeout(
        Duration::from_secs(2),
        hub.session(tokio_stream::wrappers::ReceiverStream::new(second_messages)),
    )
    .await
    .expect("bounded replay must not block the session response")
    .expect("replacement session")
    .into_inner();
    let welcome = second_session
        .message()
        .await
        .expect("replacement welcome")
        .expect("welcome");
    assert!(matches!(
        welcome.msg,
        Some(proto::hub_message::Msg::Welcome(proto::Welcome {
            heartbeat_ms: 15_000,
            ..
        }))
    ));
    let mut replayed = Vec::new();
    for _ in 0..replay_total {
        let message = tokio::time::timeout(Duration::from_secs(2), second_session.message())
            .await
            .expect("replay command arrives")
            .expect("replay stream remains open")
            .expect("replay command");
        let Some(proto::hub_message::Msg::Command(command)) = message.msg else {
            panic!("expected a replayed command");
        };
        replayed.push(command.command_id);
    }
    replayed.sort();
    let mut expected = (0..replay_total)
        .map(|index| format!("replay-{index:02}"))
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(replayed, expected);
    second_sender
        .send(proto::NodeMessage {
            msg: Some(proto::node_message::Msg::Heartbeat(proto::Heartbeat {
                capacity: Some(proto::Capacity {
                    cores_total: 8,
                    cores_free: 2.5,
                    memory_free: 2048,
                    disk_free: 4096,
                    running: 2,
                    on_battery: false,
                    battery_known: true,
                    local_minute_of_day: Some(10 * 60),
                    thermal: "fair".into(),
                }),
                presence: proto::OwnerPresence::Away as i32,
                toolchains: Vec::new(),
                repos: Vec::new(),
                running: Vec::new(),
                idle_for_ms: 47_000,
            })),
        })
        .await
        .expect("send heartbeat");
    drop(second_sender);
    let second_ended = tokio::time::timeout(Duration::from_secs(2), second_session.message())
        .await
        .expect("session closes after client disconnect")
        .expect("session result");
    assert!(second_ended.is_none());
    let path = directory.path().join("journal.sqlite");
    let updated = tokio::task::spawn_blocking(move || {
        Journal::open(path)
            .expect("journal")
            .load_node(id)
            .expect("load node")
            .expect("node")
    })
    .await
    .expect("heartbeat read worker");
    assert_eq!(updated.state, NodeState::Active);
    assert_eq!(updated.capacity.cores_total, 8);
    assert_eq!(updated.capacity.cores_free, 2.5);
    assert_eq!(updated.capacity.running, 2);
    assert_eq!(
        updated.presence,
        OwnerPresence::Away {
            idle_for: Duration::from_secs(47),
        }
    );
    assert!(updated.last_seen > Timestamp(0));
    let ended = tokio::time::timeout(Duration::from_secs(2), first_session.message())
        .await
        .expect("superseded stream closed")
        .expect("stream status");
    assert!(ended.is_none());

    let payload = b"loopback blob".to_vec();
    let digest = <[u8; 32]>::from(sha2::Sha256::digest(&payload));
    let chunks = request_stream(vec![
        Ok::<_, Status>(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: digest.to_vec(),
                size: payload.len() as u64,
            })),
        }),
        Ok(proto::Chunk {
            body: Some(chunk::Body::Data(payload.clone())),
        }),
    ]);
    let stored = hub.put_blob(chunks).await.expect("put blob").into_inner();
    assert_eq!(stored.sha256, digest);
    assert_eq!(stored.size, payload.len() as u64);
    let received = hub
        .get_blob(proto::BlobRef {
            sha256: digest.to_vec(),
            size: payload.len() as u64,
        })
        .await
        .expect("get blob")
        .into_inner()
        .collect::<Result<Vec<_>, _>>()
        .await
        .expect("read chunks");
    let received_header = received.first().and_then(|part| match part.body.as_ref() {
        Some(chunk::Body::Header(header)) => Some(header),
        _ => None,
    });
    assert!(
        received_header.is_some(),
        "GetBlob must begin with a header"
    );
    if let Some(header) = received_header {
        assert_eq!(header.sha256, digest);
        assert_eq!(header.size, payload.len() as u64);
    }
    assert!(
        received
            .iter()
            .skip(1)
            .all(|part| match part.body.as_ref() {
                Some(chunk::Body::Data(data)) => data.len() <= super::MAX_CHUNK_SIZE,
                _ => false,
            })
    );
    let restored: Vec<u8> = received
        .into_iter()
        .skip(1)
        .flat_map(|part| match part.body {
            Some(chunk::Body::Data(data)) => data,
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(restored, payload);

    let mismatched = request_stream(vec![
        Ok::<_, Status>(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: vec![0; 32],
                size: payload.len() as u64,
            })),
        }),
        Ok(proto::Chunk {
            body: Some(chunk::Body::Data(payload.clone())),
        }),
    ]);
    let error = hub
        .put_blob(mismatched)
        .await
        .expect_err("digest mismatch rejected");
    assert_eq!(error.code(), Code::InvalidArgument);

    let extra_bytes = request_stream(vec![
        Ok::<_, Status>(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: digest.to_vec(),
                size: payload.len() as u64 - 1,
            })),
        }),
        Ok(proto::Chunk {
            body: Some(chunk::Body::Data(payload.clone())),
        }),
    ]);
    assert_eq!(
        hub.put_blob(extra_bytes)
            .await
            .expect_err("data beyond declared size rejected")
            .code(),
        Code::InvalidArgument
    );

    let missing_header = request_stream(vec![Ok::<_, Status>(proto::Chunk {
        body: Some(chunk::Body::Data(payload.clone())),
    })]);
    assert_eq!(
        hub.put_blob(missing_header)
            .await
            .expect_err("missing header rejected")
            .code(),
        Code::InvalidArgument
    );

    let repeated_header = request_stream(vec![
        Ok::<_, Status>(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: digest.to_vec(),
                size: payload.len() as u64,
            })),
        }),
        Ok(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: digest.to_vec(),
                size: payload.len() as u64,
            })),
        }),
    ]);
    assert_eq!(
        hub.put_blob(repeated_header)
            .await
            .expect_err("extra header rejected")
            .code(),
        Code::InvalidArgument
    );

    let incomplete = request_stream(vec![Ok::<_, Status>(proto::Chunk {
        body: Some(chunk::Body::Header(proto::BlobHeader {
            sha256: digest.to_vec(),
            size: payload.len() as u64,
        })),
    })]);
    assert_eq!(
        hub.put_blob(incomplete)
            .await
            .expect_err("short blob rejected")
            .code(),
        Code::InvalidArgument
    );

    let empty_data = request_stream(vec![
        Ok::<_, Status>(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: vec![0; 32],
                size: 0,
            })),
        }),
        Ok(proto::Chunk {
            body: Some(chunk::Body::Data(Vec::new())),
        }),
    ]);
    assert_eq!(
        hub.put_blob(empty_data)
            .await
            .expect_err("empty data chunk rejected")
            .code(),
        Code::InvalidArgument
    );

    let too_large = request_stream(vec![Ok::<_, Status>(proto::Chunk {
        body: Some(chunk::Body::Header(proto::BlobHeader {
            sha256: vec![0; 32],
            size: super::MAX_BLOB_SIZE + 1,
        })),
    })]);
    assert_eq!(
        hub.put_blob(too_large)
            .await
            .expect_err("oversized blob rejected")
            .code(),
        Code::ResourceExhausted
    );

    let oversized_chunk_data = vec![1; super::MAX_CHUNK_SIZE + 1];
    let oversized_chunk = request_stream(vec![
        Ok::<_, Status>(proto::Chunk {
            body: Some(chunk::Body::Header(proto::BlobHeader {
                sha256: vec![0; 32],
                size: oversized_chunk_data.len() as u64,
            })),
        }),
        Ok(proto::Chunk {
            body: Some(chunk::Body::Data(oversized_chunk_data)),
        }),
    ]);
    assert_eq!(
        hub.put_blob(oversized_chunk)
            .await
            .expect_err("oversized chunk rejected")
            .code(),
        Code::ResourceExhausted
    );

    let absent = hub
        .get_blob(proto::BlobRef {
            sha256: vec![0; 32],
            size: payload.len() as u64,
        })
        .await;
    let absent = match absent {
        Err(error) => error,
        Ok(_) => panic!("mismatched upload was persisted"),
    };
    assert_eq!(absent.code(), Code::NotFound);
    tokio::task::spawn_blocking({
        let path = directory.path().join("journal.sqlite");
        move || {
            Journal::open(path)
                .expect("journal")
                .remove_node(id, Timestamp::now())
                .expect("remove node")
        }
    })
    .await
    .expect("removal worker");
    let removed = hub
        .get_blob(proto::BlobRef {
            sha256: digest.to_vec(),
            size: payload.len() as u64,
        })
        .await;
    let removed = match removed {
        Err(error) => error,
        Ok(_) => panic!("removed node certificate was accepted"),
    };
    assert_eq!(removed.code(), Code::Unauthenticated);
    let removed_session = hub
        .session(request_stream(vec![Ok::<_, Status>(proto::NodeMessage {
            msg: Some(proto::node_message::Msg::Hello(proto::Hello {
                node_id: enrolled.node_id,
                version: "test".into(),
                workspaces: Vec::new(),
                pending: Vec::new(),
                policy: None,
            })),
        })]))
        .await
        .expect_err("removed node session rejected");
    assert_eq!(removed_session.code(), Code::Unauthenticated);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn cancellation_before_journal_insertion_prevents_command_dispatch() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let plane = NodePlane::new(
        directory.path().join("journal.sqlite"),
        directory.path().join("blobs"),
        HubCa::generate().expect("CA"),
    )
    .await
    .expect("node plane");
    let node_id = NodeId::new();
    let command_id = "cancel-before-command-journal-insert";

    // The node is offline, so only the in-process cancellation intent can close
    // the gap before execute_command stores its durable row.
    assert!(matches!(
        plane
            .cancel_command(node_id, command_id, "subtask cancelled")
            .await,
        Err(super::NodePlaneError::Offline)
    ));
    let result = plane
        .execute_command(
            node_id,
            proto::Command {
                command_id: command_id.into(),
                subtask_id: tagteam_core::SubtaskId::new().to_string(),
                workspace: "workspace".into(),
                op: Some(proto::command::Op::Cleanup(proto::Cleanup {
                    keep_cache: false,
                })),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("cancelled command returns before any session lookup");
    assert!(matches!(
        result.result,
        Some(proto::command_result::Result::Error(error))
            if error.code == proto::ErrorCode::Cancelled as i32
    ));
    let journal = plane
        .inner
        .journal
        .lock()
        .expect("journal lock remains available");
    assert!(
        journal
            .pending_commands(node_id)
            .expect("pending commands")
            .is_empty(),
        "a cancellation intent must prevent a replayable command row"
    );
}

#[test]
fn node_policy_conversion_preserves_owner_limits_and_rejects_invalid_values() {
    let policy = policy_from_proto(proto::NodePolicy {
        only_when_idle: true,
        idle_after_ms: 90_000,
        on_owner_return: "hand_back".into(),
        max_cores: Some(4),
        max_memory: Some(8 * 1024 * 1024),
        max_subtasks: 2,
        hours: vec![proto::HourRange {
            start_minutes: 540,
            end_minutes: 1020,
        }],
        never_on_battery: true,
        extra_hosts: vec!["git.example".into()],
    })
    .expect("valid policy");
    assert!(policy.only_when_idle);
    assert_eq!(policy.idle_after, Duration::from_secs(90));
    assert_eq!(policy.on_owner_return, tagteam_core::OnReturn::HandBack);
    assert_eq!(policy.max_cores, Some(4));
    assert_eq!(
        policy.max_memory,
        Some(tagteam_core::Bytes(8 * 1024 * 1024))
    );
    assert_eq!(policy.max_subtasks, 2);
    assert_eq!(policy.extra_hosts, vec!["git.example"]);

    let mut invalid = proto::NodePolicy {
        on_owner_return: "pause".into(),
        max_subtasks: 1,
        ..proto::NodePolicy::default()
    };
    invalid.max_subtasks = 0;
    assert!(policy_from_proto(invalid).is_err());

    let mut invalid = proto::NodePolicy {
        on_owner_return: "unknown".into(),
        max_subtasks: 1,
        ..proto::NodePolicy::default()
    };
    invalid.on_owner_return = "unknown".into();
    assert!(policy_from_proto(invalid).is_err());
}
