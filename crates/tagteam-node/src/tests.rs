//! Loopback enrollment and session tests against the hub node plane.
//!
//! Temporary journals and deterministic heartbeats keep these transport tests isolated and
//! fast; no test depends on the production heartbeat interval elapsing.

use std::{net::SocketAddr, time::Duration};

use data_encoding::BASE32_NOPAD;
use tagteam_core::{Thermal, Timestamp};
use tagteam_hub::{blobs::BlobStore, journal::Journal, nodes::NodePlane};
use tagteam_pki::{CertificateFingerprint, HubCa, Invite};
use tokio::{sync::oneshot, time::timeout};

use crate::{
    BundleSource, FileIdentityStore, HeartbeatSource, HubBundleSource, IdentityStore,
    NodeEnrollment, NodeSession, SessionEvent,
};

struct StaticHeartbeat;

impl HeartbeatSource for StaticHeartbeat {
    fn heartbeat(&self) -> tagteam_proto::v1::Heartbeat {
        tagteam_proto::v1::Heartbeat {
            capacity: Some(tagteam_proto::v1::Capacity {
                cores_total: 8,
                cores_free: 3.5,
                memory_free: 4096,
                disk_free: 8192,
                running: 0,
                on_battery: false,
                thermal: "fair".into(),
            }),
            presence: tagteam_proto::v1::OwnerPresence::Away as i32,
            toolchains: Vec::new(),
            repos: Vec::new(),
            running: Vec::new(),
        }
    }
}

async fn start_node_plane(
    directory: &tempfile::TempDir,
    ca: HubCa,
) -> (SocketAddr, oneshot::Sender<()>) {
    let plane = NodePlane::new(
        directory.path().join("journal.sqlite"),
        directory.path().join("blobs"),
        ca,
    )
    .await
    .expect("node plane initializes");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener binds");
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

async fn create_invite(
    directory: &tempfile::TempDir,
    address: SocketAddr,
    fingerprint: CertificateFingerprint,
    code: [u8; 32],
) -> String {
    Journal::open(directory.path().join("journal.sqlite"))
        .expect("open journal")
        .create_invite(
            &BASE32_NOPAD.encode(&code),
            "node enrollment",
            Timestamp(i64::MAX),
        )
        .expect("create invite");
    Invite::new(format!("https://{address}"), fingerprint, code)
        .expect("construct invite")
        .encode()
}

#[tokio::test]
async fn enrollment_verifies_pinned_ca_and_returns_matching_certificate_identity() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let ca = HubCa::generate().expect("hub CA");
    let fingerprint = ca.fingerprint();
    let (address, shutdown) = start_node_plane(&directory, ca).await;
    let invite = create_invite(&directory, address, fingerprint, [7; 32]).await;
    let identity = NodeEnrollment::enroll(
        &invite,
        "loopback node",
        tagteam_proto::v1::Platform {
            os: tagteam_proto::v1::Os::Linux as i32,
            arch: tagteam_proto::v1::Arch::X64 as i32,
            os_version: "test".into(),
        },
    )
    .await
    .expect("enrollment succeeds");
    assert!(!identity.private_key_der.is_empty());
    assert_eq!(identity.hub_fingerprint, fingerprint);
    assert_eq!(identity.hub_address, format!("https://{address}"));
    let identity_dir = directory.path().join("node-data");
    let identity_store = FileIdentityStore::new(&identity_dir);
    identity_store
        .persist(&identity)
        .expect("identity persists");
    let (stored_invite, restored) = identity_store
        .load()
        .expect("identity loads")
        .expect("identity exists");
    assert_eq!(stored_invite, invite);
    assert_eq!(restored.node_id, identity.node_id);
    assert_eq!(restored.certificate_der, identity.certificate_der);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&identity_dir)
                .expect("data dir metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(identity_dir.join("identity.json"))
                .expect("identity metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let saved_node = Journal::open(directory.path().join("journal.sqlite"))
        .expect("open journal")
        .load_node(identity.node_id)
        .expect("load enrolled node")
        .expect("node exists");
    assert_eq!(
        saved_node.cert_sha256,
        *CertificateFingerprint::from_certificate(&identity.certificate_der).as_bytes()
    );
    let _ = shutdown.send(());
}

#[tokio::test]
async fn wrong_pinned_ca_fails_before_the_invite_is_consumed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let ca = HubCa::generate().expect("hub CA");
    let (address, shutdown) = start_node_plane(&directory, ca).await;
    let wrong_ca = HubCa::generate().expect("foreign CA");
    let invite_code = [9; 32];
    let invite = create_invite(&directory, address, wrong_ca.fingerprint(), invite_code).await;
    let result = NodeEnrollment::enroll(
        &invite,
        "wrong pin node",
        tagteam_proto::v1::Platform {
            os: tagteam_proto::v1::Os::Linux as i32,
            arch: tagteam_proto::v1::Arch::X64 as i32,
            os_version: "test".into(),
        },
    )
    .await;
    assert!(result.is_err());
    let invite_row = Journal::open(directory.path().join("journal.sqlite"))
        .expect("open journal")
        .load_invite(&BASE32_NOPAD.encode(&invite_code))
        .expect("load invite")
        .expect("invite still exists");
    assert!(invite_row.used_at.is_none());
    let _ = shutdown.send(());
}

#[tokio::test]
async fn authenticated_bundle_source_downloads_and_rejects_wrong_references() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let ca = HubCa::generate().expect("hub CA");
    let fingerprint = ca.fingerprint();
    let (address, shutdown) = start_node_plane(&directory, ca).await;
    let invite = create_invite(&directory, address, fingerprint, [13; 32]).await;
    let identity = NodeEnrollment::enroll(
        &invite,
        "blob node",
        tagteam_proto::v1::Platform {
            os: tagteam_proto::v1::Os::Linux as i32,
            arch: tagteam_proto::v1::Arch::X64 as i32,
            os_version: "test".into(),
        },
    )
    .await
    .expect("enrollment succeeds");

    let bytes = b"verified workspace bundle";
    let reference = BlobStore::open(directory.path().join("blobs"))
        .expect("blob store opens")
        .put(bytes)
        .expect("blob is seeded");
    let source = HubBundleSource::new(&identity)
        .await
        .expect("authenticated hub source connects");
    let wire_reference = tagteam_proto::v1::BlobRef {
        sha256: reference.sha256.to_vec(),
        size: reference.size,
    };
    assert_eq!(
        source
            .bundle_bytes(&wire_reference)
            .await
            .expect("bundle downloads and verifies"),
        bytes
    );

    let upload_bytes = vec![0x5a; 2 * 1024 * 1024 + 17];
    let uploaded = source
        .store_bundle(upload_bytes.clone())
        .await
        .expect("bundle uploads over authenticated hub transport");
    assert_eq!(uploaded.size, upload_bytes.len() as u64);
    assert_eq!(
        source
            .bundle_bytes(&uploaded)
            .await
            .expect("uploaded bundle downloads and verifies"),
        upload_bytes
    );

    let mut wrong_digest = wire_reference.clone();
    wrong_digest.sha256[0] ^= 1;
    assert!(source.bundle_bytes(&wrong_digest).await.is_err());

    let mut wrong_size = wire_reference;
    wrong_size.size += 1;
    assert!(source.bundle_bytes(&wrong_size).await.is_err());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn session_hello_welcome_and_immediate_heartbeat_reach_the_journal() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let ca = HubCa::generate().expect("hub CA");
    let fingerprint = ca.fingerprint();
    let (address, shutdown) = start_node_plane(&directory, ca).await;
    let invite = create_invite(&directory, address, fingerprint, [11; 32]).await;
    let identity = NodeEnrollment::enroll(
        &invite,
        "session node",
        tagteam_proto::v1::Platform {
            os: tagteam_proto::v1::Os::Linux as i32,
            arch: tagteam_proto::v1::Arch::X64 as i32,
            os_version: "test".into(),
        },
    )
    .await
    .expect("enrollment succeeds");
    let node_id = identity.node_id;
    let session = NodeSession::new(identity, StaticHeartbeat)
        .hello_state("test-version", vec!["workspace-a".into()], Vec::new())
        .start();
    let mut session = session;
    let welcome = timeout(Duration::from_secs(3), session.next_event())
        .await
        .expect("Welcome arrives promptly");
    assert!(
        matches!(welcome, Some(SessionEvent::Welcome(value)) if !value.session_id.is_empty() && value.heartbeat_ms > 0)
    );
    let journal_path = directory.path().join("journal.sqlite");
    let heartbeat = timeout(Duration::from_secs(3), async move {
        loop {
            let path = journal_path.clone();
            let node = tokio::task::spawn_blocking(move || {
                Journal::open(path)
                    .expect("open journal")
                    .load_node(node_id)
                    .expect("load node")
                    .expect("node exists")
            })
            .await
            .expect("journal query completes");
            if node.capacity.cores_total == 8 {
                break node;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("immediate heartbeat is journaled without waiting for the interval");
    assert_eq!(heartbeat.capacity.cores_free, 3.5);
    assert_eq!(heartbeat.capacity.thermal, Thermal::Fair);
    session.shutdown();
    let _ = shutdown.send(());
}
