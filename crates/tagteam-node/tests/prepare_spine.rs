//! Loopback proof that a placed subtask is bundled, uploaded and prepared by a real node.
//!
//! The hub and node communicate over mutual TLS. The runner is intentionally absent:
//! this test establishes the preparation boundary only and spends no model tokens.

use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    process::Command,
    sync::Arc,
    time::Duration,
};

use tagteam_core::{
    Bytes, Capacity, Limits, MainSessionId, Needs, SubtaskSpec, SubtaskState, Thermal,
    local_api::{DispatchInput, LocalRequest, LocalResponseBody},
};
use tagteam_hub::{config::HubConfig, service::HubService};
use tagteam_node::{
    CommandJournal, CommandRunner, HeartbeatSource, HubBundleSource, NodeEnrollment, NodeSession,
    SessionEvent,
};
use tagteam_pki::HubCa;
use tokio::sync::oneshot;

struct TestHeartbeat;

impl HeartbeatSource for TestHeartbeat {
    fn heartbeat(&self) -> tagteam_proto::v1::Heartbeat {
        tagteam_proto::v1::Heartbeat {
            capacity: Some(tagteam_proto::v1::Capacity {
                cores_total: 8,
                cores_free: 8.0,
                memory_free: Bytes::gib(16).0,
                disk_free: Bytes::gib(64).0,
                running: 0,
                on_battery: false,
                thermal: "nominal".into(),
            }),
            presence: tagteam_proto::v1::OwnerPresence::Away as i32,
            toolchains: Vec::new(),
            repos: Vec::new(),
            running: Vec::new(),
        }
    }
}

fn git(repository: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const LARGE_FIXTURE_BYTES: usize = 3 * 1024 * 1024;

fn large_fixture() -> Vec<u8> {
    let mut state = 0x5eed_cafe_f00d_beef_u64;
    let mut bytes = vec![0_u8; LARGE_FIXTURE_BYTES];
    for byte in &mut bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    bytes
}

fn repository(directory: &tempfile::TempDir) -> std::path::PathBuf {
    let repository = directory.path().join("repository");
    fs::create_dir_all(&repository).expect("repository directory");
    git(&repository, &["init", "-q"]);
    git(&repository, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&repository, &["config", "user.name", "Tag Team Spine"]);
    git(
        &repository,
        &["config", "user.email", "tagteam@example.invalid"],
    );
    fs::write(repository.join("tracked.txt"), "prepared on the node\n").expect("tracked file");
    // Exceed common OS argument limits once base64-encoded. A source bundle
    // must travel over the confined helper's stdin, not its command line.
    fs::write(repository.join("large.bin"), large_fixture()).expect("large fixture");
    git(&repository, &["add", "-A"]);
    git(&repository, &["commit", "-qm", "base"]);
    repository
}

fn config(data_dir: std::path::PathBuf, address: SocketAddr) -> HubConfig {
    HubConfig {
        listen: "127.0.0.1:0".parse().expect("listen address"),
        public_address: format!("https://{address}"),
        data_dir,
        max_concurrent_per_account: 1,
        runner_program: "node".into(),
        runner_args: Vec::new(),
        limits: Limits::default(),
        heartbeat_timeout: Duration::from_secs(90),
    }
}

async fn wait_for_capacity(service: &HubService) {
    for _ in 0..100 {
        let response = service
            .handle(LocalRequest::Nodes)
            .await
            .expect("node status");
        let LocalResponseBody::Nodes(nodes) = response else {
            panic!("unexpected nodes response");
        };
        if nodes.first().is_some_and(|node| {
            node.node.capacity.cores_total == 8
                && node.node.capacity
                    == Capacity {
                        cores_total: 8,
                        cores_free: 8.0,
                        memory_free: Bytes::gib(16),
                        disk_free: Bytes::gib(64),
                        running: 0,
                        on_battery: false,
                        thermal: Thermal::Nominal,
                    }
        }) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("node heartbeat did not reach the hub");
}

#[tokio::test]
async fn dispatch_prepares_a_private_workspace_over_mtls() {
    if cfg!(target_os = "linux")
        && (!Path::new("/usr/bin/systemd-run").exists() || !Path::new("/usr/bin/bwrap").exists())
    {
        eprintln!("sandbox tools unavailable; workspace preparation spine is skipped");
        return;
    }

    // Unix-domain socket paths are length-limited; the platform temporary
    // directory can exceed that limit before the workspace suffix is added.
    let short_root = if Path::new("/tmp").is_dir() {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let directory = tempfile::Builder::new()
        .prefix("tt-prepare-")
        .tempdir_in(short_root)
        .expect("temporary directory");
    let repository = repository(&directory);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("listener");
    let address = listener.local_addr().expect("listener address");
    let ca = Arc::new(HubCa::generate().expect("hub CA"));
    let service = HubService::open(
        &config(directory.path().join("hub-data"), address),
        Arc::clone(&ca),
    )
    .await
    .expect("hub service");
    let plane = service.nodes().clone();
    let (shutdown, shutdown_receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        plane
            .serve(listener, async move {
                let _ = shutdown_receiver.await;
            })
            .await
    });

    let invite = match service
        .handle(LocalRequest::Invite {
            name: "prepare-node".into(),
            ttl_seconds: 60,
        })
        .await
        .expect("invite")
    {
        LocalResponseBody::Invited(invite) => invite.invite,
        other => panic!("unexpected invite response: {other:?}"),
    };
    let identity = NodeEnrollment::enroll(
        &invite,
        "prepare-node",
        tagteam_proto::v1::Platform {
            os: tagteam_proto::v1::Os::Macos as i32,
            arch: tagteam_proto::v1::Arch::Arm64 as i32,
            os_version: "loopback".into(),
        },
    )
    .await
    .expect("enrollment");
    let node_id = identity.node_id;
    service
        .handle(LocalRequest::Approve {
            name: "prepare-node".into(),
        })
        .await
        .expect("approve node");

    let node_data = directory.path().join("node-data");
    fs::create_dir_all(&node_data).expect("node data directory");
    let journal = CommandJournal::open(node_data.join("commands.json")).expect("command journal");
    let source = HubBundleSource::connect(&identity)
        .await
        .expect("blob source");
    let runner = CommandRunner::new(&node_data, journal.clone(), source)
        .with_executable(env!("CARGO_BIN_EXE_tagteam-node"));
    let session = NodeSession::new(identity, TestHeartbeat)
        .pending_source(journal)
        .start();
    let sender = session.sender();
    let mut events = session;
    let node_loop = tokio::spawn(async move {
        while let Some(event) = events.next_event().await {
            match event {
                SessionEvent::Command(command) => runner.dispatch(*command, sender.clone()),
                SessionEvent::Cancel(cancel) => runner.cancel(&cancel.command_id),
                SessionEvent::Drain(_) | SessionEvent::Terminated(_) => break,
                SessionEvent::Welcome(_) | SessionEvent::Reconnecting { .. } => {}
            }
        }
    });

    for _ in 0..100 {
        if service.nodes().is_online(node_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(service.nodes().is_online(node_id), "node session connected");
    wait_for_capacity(&service).await;

    let dispatch = match service
        .handle(LocalRequest::Dispatch(DispatchInput {
            main_session: MainSessionId::new(),
            claude_session: Some("spine-session".into()),
            repo: repository.clone(),
            subtasks: vec![SubtaskSpec {
                title: "Prepare a workspace".into(),
                prompt: "No runner is launched in this checkpoint.".into(),
                done_when: vec!["true".into()],
                repo: repository.clone(),
                base_ref: "main".into(),
                include_uncommitted: false,
                needs: Needs::default(),
                limits: Limits::default(),
                egress: Vec::new(),
                node_hint: None,
            }],
            spread: false,
        }))
        .await
        .expect("dispatch")
    {
        LocalResponseBody::Dispatched(dispatch) => dispatch,
        other => panic!("unexpected dispatch response: {other:?}"),
    };
    let subtask = dispatch.subtasks[0].0;

    let mut final_state = None;
    for _ in 0..400 {
        let status = match service
            .handle(LocalRequest::Status {
                ids: Some(vec![subtask]),
            })
            .await
            .expect("status")
        {
            LocalResponseBody::Status(status) => status,
            other => panic!("unexpected status response: {other:?}"),
        };
        let current = &status.subtasks[0];
        if current.state == "running" {
            final_state = Some(SubtaskState::Running);
            break;
        }
        if current.state == "failed" {
            final_state = Some(SubtaskState::Failed {
                reason: tagteam_core::FailReason::Bundle(
                    current.last_progress.clone().unwrap_or_default(),
                ),
            });
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        final_state,
        Some(SubtaskState::Running),
        "workspace must reach running after preparation: {dispatch:?}"
    );
    let workspace = node_data
        .join("workspaces")
        .join(subtask.to_string())
        .join("repo");
    assert_eq!(
        fs::read(workspace.join("tracked.txt")).expect("prepared file"),
        b"prepared on the node\n"
    );
    assert_eq!(
        fs::read(workspace.join("large.bin")).expect("large prepared file"),
        large_fixture()
    );
    assert!(workspace.join(".git").is_dir());
    assert!(!workspace.join(".tagteam-input.bundle").exists());

    node_loop.abort();
    let _ = shutdown.send(());
    let _ = server.await;
}
