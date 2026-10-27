//! Loopback tests for the private local API and persistent CA lifecycle.

use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use tagteam_core::local_api::{LocalRequest, LocalResponseBody};
use tagteam_pki::{HubCa, Invite};
use tempfile::TempDir;
use tokio::sync::watch;

use crate::{config::HubConfig, service::HubService};

use super::{
    client::LocalClient,
    server::{self, LocalServerError},
};

fn config(directory: &TempDir) -> HubConfig {
    HubConfig {
        listen: "127.0.0.1:0".parse().expect("listen address"),
        public_address: "https://127.0.0.1:7443".into(),
        data_dir: directory.path().to_path_buf(),
        max_concurrent_per_account: 2,
        runner_program: "node".into(),
        runner_args: Vec::new(),
        limits: tagteam_core::Limits::default(),
        heartbeat_timeout: Duration::from_secs(90),
    }
}

async fn connect_ready(path: &std::path::Path) -> LocalClient {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match LocalClient::connect(path).await {
            Ok(mut client) => match client.request(LocalRequest::Ping).await {
                Ok(LocalResponseBody::Pong) => return client,
                Ok(other) => panic!("unexpected readiness response: {other:?}"),
                Err(error) if tokio::time::Instant::now() < deadline => {
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("local socket readiness failed: {error}"),
            },
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("local socket did not start: {error}"),
        }
    }
}

async fn start(
    config: &HubConfig,
    ca: Arc<HubCa>,
) -> (
    HubService,
    watch::Sender<bool>,
    LocalClient,
    tokio::task::JoinHandle<Result<(), LocalServerError>>,
) {
    let service = HubService::open(config, ca).await.expect("service");
    let socket = service.paths().socket.clone();
    let (shutdown, receiver) = watch::channel(false);
    let runner_service = service.clone();
    let runner = tokio::spawn(async move { server::serve(socket, runner_service, receiver).await });
    let client = connect_ready(&service.paths().socket).await;
    (service, shutdown, client, runner)
}

#[tokio::test]
async fn local_client_ping_invite_and_persistent_ca_restart() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = config(&directory);
    let paths = config.prepare_data_dir().expect("data paths");
    let ca = Arc::new(
        HubCa::load_or_generate(&paths.ca_certificate, &paths.ca_private_key).expect("first CA"),
    );
    let fingerprint = ca.fingerprint();
    let (service, shutdown, mut client, runner) = start(&config, ca).await;
    assert_eq!(
        fs::metadata(&paths.ca_private_key)
            .expect("key metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&paths.socket)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    assert_eq!(
        client.request(LocalRequest::Ping).await.expect("ping"),
        LocalResponseBody::Pong
    );
    let invited = client
        .request(LocalRequest::Invite {
            name: "mini".into(),
            ttl_seconds: 600,
        })
        .await
        .expect("invite");
    let LocalResponseBody::Invited(invited) = invited else {
        panic!("unexpected invite response");
    };
    let decoded = Invite::decode(&invited.invite).expect("decode invite");
    assert_eq!(decoded.address, "https://127.0.0.1:7443");
    assert_eq!(decoded.hub_fingerprint, fingerprint);
    assert_eq!(invited.expires_in_seconds, 600);

    shutdown.send_replace(true);
    runner.await.expect("server join").expect("server shutdown");
    assert!(!paths.socket.exists(), "socket is removed on shutdown");
    drop(service);

    let reloaded =
        HubCa::load(&paths.ca_certificate, &paths.ca_private_key).expect("reload CA after restart");
    assert_eq!(reloaded.fingerprint(), fingerprint);
}

#[tokio::test]
async fn dropped_raw_peer_does_not_stop_the_daemon() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = config(&directory);
    let ca = Arc::new(HubCa::generate().expect("CA"));
    let (service, shutdown, mut client, runner) = start(&config, ca).await;

    // A peer may close after accept but before the daemon reads its credentials.
    // That rejected connection must not terminate the accept loop.
    for _ in 0..8 {
        let stream = tokio::net::UnixStream::connect(&service.paths().socket)
            .await
            .expect("raw connection");
        drop(stream);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        client.request(LocalRequest::Ping).await.expect("ping"),
        LocalResponseBody::Pong
    );
    shutdown.send_replace(true);
    runner.await.expect("server join").expect("server shutdown");
}

#[tokio::test]
async fn unsupported_dispatch_is_a_structured_daemon_error() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = config(&directory);
    let ca = Arc::new(HubCa::generate().expect("CA"));
    let (_service, shutdown, mut client, runner) = start(&config, ca).await;
    let error = client
        .request(LocalRequest::Dispatch(
            tagteam_core::local_api::DispatchInput {
                main_session: tagteam_core::MainSessionId::new(),
                claude_session: None,
                repo: directory.path().to_path_buf(),
                subtasks: Vec::new(),
                spread: false,
            },
        ))
        .await
        .expect_err("dispatch is not implemented");
    assert!(error.to_string().contains("orchestration"));
    shutdown.send_replace(true);
    runner.await.expect("server join").expect("server shutdown");
}

#[tokio::test]
async fn non_socket_path_is_never_removed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("hub.sock");
    fs::write(&path, b"owner data").expect("plain file");
    let config = config(&directory);
    let ca = Arc::new(HubCa::generate().expect("CA"));
    let service = HubService::open(&config, ca).await.expect("service");
    let (_shutdown, receiver) = watch::channel(false);
    let error = server::serve(&path, service, receiver)
        .await
        .expect_err("plain file rejected");
    assert!(matches!(error, LocalServerError::UnsafeSocketPath));
    assert_eq!(fs::read(&path).expect("file survives"), b"owner data");
}
