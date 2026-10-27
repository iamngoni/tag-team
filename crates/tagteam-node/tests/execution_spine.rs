//! Loopback proof that a placed subtask is prepared, executed by a runner, and
//! landed as an unmerged review branch over mTLS.
//!
//! The runner is a deterministic protocol fixture, so this test exercises the
//! real hub, node, sandbox, blob, git, and cancellation boundaries without
//! spending model tokens or touching a live machine.

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
            idle_for_ms: 0,
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

fn config(
    data_dir: std::path::PathBuf,
    address: SocketAddr,
    runner_args: Vec<String>,
) -> HubConfig {
    HubConfig {
        listen: "127.0.0.1:0".parse().expect("listen address"),
        public_address: format!("https://{address}"),
        data_dir,
        max_concurrent_per_account: 1,
        runner_program: "node".into(),
        runner_args,
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
async fn dispatch_executes_and_lands_an_unmerged_review_branch() {
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
    let owner_head = git_output(&repository, &["rev-parse", "HEAD"]);
    let owner_index = git_output(&repository, &["ls-files", "-s"]);
    let owner_status = git_output(&repository, &["status", "--porcelain=v1", "-z"]);
    let fake_runner =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runner/test-fixtures/fake-runner.mjs");
    assert!(fake_runner.is_file(), "fake runner fixture exists");
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("listener");
    let address = listener.local_addr().expect("listener address");
    let ca = Arc::new(HubCa::generate().expect("hub CA"));
    let service = HubService::open(
        &config(
            directory.path().join("hub-data"),
            address,
            vec![fake_runner.to_string_lossy().into_owned()],
        ),
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
                prompt: "Write the result file through the remote runner.".into(),
                done_when: vec!["test -f result.txt".into()],
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
    let mut last_progress = None;
    for _ in 0..1_200 {
        let status = match service
            .handle(LocalRequest::Status {
                claude_session: None,
                ids: Some(vec![subtask]),
            })
            .await
            .expect("status")
        {
            LocalResponseBody::Status(status) => status,
            other => panic!("unexpected status response: {other:?}"),
        };
        let current = &status.subtasks[0];
        last_progress = current.last_progress.clone();
        if current.state == "done" {
            final_state = Some(SubtaskState::Done);
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
        Some(SubtaskState::Done),
        "subtask must reach done; last progress: {last_progress:?}; dispatch: {dispatch:?}"
    );
    let collected = match service
        .handle(LocalRequest::Collect {
            claude_session: None,
            subtask,
            worktree: false,
        })
        .await
        .expect("collect result")
    {
        LocalResponseBody::Collected(collected) => collected,
        other => panic!("unexpected collect response: {other:?}"),
    };
    assert_eq!(collected.result.outcome, tagteam_core::Outcome::Completed);
    assert!(collected.result.summary.contains("Fake runner"));
    assert_eq!(collected.result.checks.len(), 1);
    assert!(collected.result.checks[0].passed);
    assert_eq!(collected.result.checks[0].what, "test -f result.txt");
    let branch = collected.result.branch.as_deref().expect("review branch");
    assert_eq!(
        git_output(&repository, &["show", &format!("{branch}:result.txt")]),
        "written by fake runner\n"
    );
    assert_eq!(
        git_output(
            &repository,
            &["cat-file", "-s", &format!("{branch}:large.bin")]
        )
        .trim(),
        LARGE_FIXTURE_BYTES.to_string()
    );
    assert_eq!(
        git_output(&repository, &["show", &format!("{branch}:tracked.txt")]),
        "prepared on the node\n"
    );
    let branch_head = git_output(
        &repository,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    );
    assert!(
        git_status(
            &repository,
            &[
                "merge-base",
                "--is-ancestor",
                owner_head.trim(),
                branch_head.trim(),
            ],
        ),
        "review branch must descend from the recorded snapshot base"
    );
    assert!(
        !git_status(
            &repository,
            &[
                "merge-base",
                "--is-ancestor",
                branch_head.trim(),
                owner_head.trim(),
            ],
        ),
        "result must never be merged into the owner branch"
    );
    assert_eq!(
        git_output(&repository, &["rev-parse", "HEAD"]),
        owner_head,
        "owner HEAD must not move"
    );
    assert_eq!(
        git_output(&repository, &["ls-files", "-s"]),
        owner_index,
        "owner index must not change"
    );
    assert_eq!(
        git_output(&repository, &["status", "--porcelain=v1", "-z"]),
        owner_status,
        "owner working tree must not change"
    );
    assert_eq!(
        git_output(&repository, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );

    let cancellation = match service
        .handle(LocalRequest::Dispatch(DispatchInput {
            main_session: MainSessionId::new(),
            claude_session: Some("spine-cancellation".into()),
            repo: repository.clone(),
            subtasks: vec![SubtaskSpec {
                title: "Cancel a running subtask".into(),
                prompt: "Exercise cancellation while the runner is active.".into(),
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
        .expect("cancellation dispatch")
    {
        LocalResponseBody::Dispatched(dispatch) => dispatch,
        other => panic!("unexpected cancellation dispatch response: {other:?}"),
    };
    let cancel_subtask = cancellation.subtasks[0].0;
    let mut cancellation_branch = None;
    let mut saw_progress = false;
    for _ in 0..400 {
        let status = match service
            .handle(LocalRequest::Status {
                claude_session: None,
                ids: Some(vec![cancel_subtask]),
            })
            .await
            .expect("cancellation status")
        {
            LocalResponseBody::Status(status) => status,
            other => panic!("unexpected cancellation status response: {other:?}"),
        };
        let current = &status.subtasks[0];
        cancellation_branch = current.branch.clone();
        if current.last_progress.as_deref() == Some("cancel-ready") {
            saw_progress = true;
            break;
        }
        assert_ne!(
            current.state, "failed",
            "runner failed before cancellation: {:?}",
            current.last_progress
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(saw_progress, "runner must reach the cancellation fixture");
    tokio::time::sleep(Duration::from_millis(700)).await;
    service
        .handle(LocalRequest::Cancel {
            claude_session: None,
            subtask: cancel_subtask,
            reason: "loopback cancellation proof".into(),
        })
        .await
        .expect("cancel running subtask");
    let mut cancelled = false;
    for _ in 0..400 {
        let status = match service
            .handle(LocalRequest::Status {
                claude_session: None,
                ids: Some(vec![cancel_subtask]),
            })
            .await
            .expect("cancelled status")
        {
            LocalResponseBody::Status(status) => status,
            other => panic!("unexpected cancelled status response: {other:?}"),
        };
        if status.subtasks[0].state == "cancelled" {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(cancelled, "cancelled subtask must remain cancelled");
    let branch = cancellation_branch.expect("cancellation branch name");
    assert!(
        !git_status(
            &repository,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}")
            ],
        ),
        "cancelled work must not create a review branch"
    );

    let check_cancellation = match service
        .handle(LocalRequest::Dispatch(DispatchInput {
            main_session: MainSessionId::new(),
            claude_session: Some("spine-check-cancellation".into()),
            repo: repository.clone(),
            subtasks: vec![SubtaskSpec {
                title: "Cancel during the authoritative check".into(),
                prompt: "cancel-during-check".into(),
                done_when: vec!["touch check-started && sleep 1 && true".into()],
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
        .expect("check cancellation dispatch")
    {
        LocalResponseBody::Dispatched(dispatch) => dispatch,
        other => panic!("unexpected check cancellation dispatch response: {other:?}"),
    };
    let check_subtask = check_cancellation.subtasks[0].0;
    let check_branch = match service
        .handle(LocalRequest::Status {
            claude_session: None,
            ids: Some(vec![check_subtask]),
        })
        .await
        .expect("check cancellation status")
    {
        LocalResponseBody::Status(status) => status.subtasks[0]
            .branch
            .clone()
            .expect("check cancellation branch"),
        other => panic!("unexpected check cancellation status response: {other:?}"),
    };
    let check_workspace = node_data
        .join("workspaces")
        .join(check_subtask.to_string())
        .join("repo");
    let mut saw_check = false;
    for _ in 0..400 {
        if check_workspace.join("check-started").is_file() {
            saw_check = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(saw_check, "authoritative check must start");
    service
        .handle(LocalRequest::Cancel {
            claude_session: None,
            subtask: check_subtask,
            reason: "cancellation during done_when proof".into(),
        })
        .await
        .expect("cancel during authoritative check");
    let mut check_cancelled = false;
    for _ in 0..400 {
        let status = match service
            .handle(LocalRequest::Status {
                claude_session: None,
                ids: Some(vec![check_subtask]),
            })
            .await
            .expect("check cancellation state")
        {
            LocalResponseBody::Status(status) => status,
            other => panic!("unexpected check cancellation state response: {other:?}"),
        };
        if status.subtasks[0].state == "cancelled" {
            check_cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(check_cancelled, "check cancellation must remain cancelled");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(
        !git_status(
            &repository,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{check_branch}")
            ],
        ),
        "cancellation during done_when must not create a review branch"
    );

    let workspace_root = node_data.join("workspaces").join(subtask.to_string());
    assert!(
        !workspace_root.exists(),
        "successful terminal work must remove its borrowed-machine workspace"
    );
    for cancelled_id in [cancel_subtask, check_subtask] {
        let cancelled_workspace = node_data.join("workspaces").join(cancelled_id.to_string());
        for _ in 0..200 {
            if !cancelled_workspace.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !cancelled_workspace.exists(),
            "cancelled terminal work must remove its borrowed-machine workspace"
        );
    }

    node_loop.abort();
    let _ = shutdown.send(());
    let _ = server.await;
}

fn git_output(repository: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).expect("git output is UTF-8")
}

fn git_status(repository: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .status()
        .expect("run git")
        .success()
}
