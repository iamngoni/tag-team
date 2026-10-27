//! Real loopback proof that a lost node's subtask is reassigned to a surviving
//! node, resumes the runner session, and lands one unmerged review branch.
//!
//! This uses the production hub/node/mTLS/sandbox path and the shared
//! deterministic runner fixture. It spends no model tokens.

use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use tagteam_core::{
    Bytes, Limits, MainSessionId, Needs, SubtaskSpec, SubtaskState,
    local_api::{DispatchInput, LocalRequest, LocalResponseBody},
    schedule::Placement,
};
use tagteam_hub::{
    config::{HubConfig, RetentionConfig},
    journal::Journal,
    service::HubService,
};
use tagteam_node::{
    CommandJournal, CommandRunner, HeartbeatSource, HubBundleSource, NodeEnrollment, NodeSession,
    SessionEvent,
};
use tagteam_pki::HubCa;
use tokio::sync::oneshot;

struct RecoveryHeartbeat;

impl HeartbeatSource for RecoveryHeartbeat {
    fn heartbeat(&self) -> tagteam_proto::v1::Heartbeat {
        tagteam_proto::v1::Heartbeat {
            capacity: Some(tagteam_proto::v1::Capacity {
                cores_total: 8,
                cores_free: 8.0,
                memory_free: Bytes::gib(16).0,
                disk_free: Bytes::gib(64).0,
                running: 0,
                on_battery: false,
                battery_known: true,
                local_minute_of_day: Some(10 * 60),
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

fn git(repository: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).expect("git output")
}

fn duplicate_identity(identity: &tagteam_node::NodeIdentity) -> tagteam_node::NodeIdentity {
    tagteam_node::NodeIdentity {
        node_id: identity.node_id,
        invite: identity.invite.clone(),
        certificate_der: identity.certificate_der.clone(),
        private_key_der: identity.private_key_der.clone(),
        hub_address: identity.hub_address.clone(),
        hub_fingerprint: identity.hub_fingerprint,
    }
}

fn make_repository(root: &Path) -> PathBuf {
    let repository = root.join("owner-repository");
    fs::create_dir_all(&repository).expect("repository");
    git(&repository, &["init", "-q"]);
    git(&repository, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&repository, &["config", "user.name", "Tag Team Recovery"]);
    git(
        &repository,
        &["config", "user.email", "tagteam@example.invalid"],
    );
    fs::write(repository.join("tracked.txt"), "owner checkout baseline\n").expect("tracked file");
    git(&repository, &["add", "-A"]);
    git(&repository, &["commit", "-qm", "owner baseline"]);
    repository
}

fn recovery_config(data_dir: PathBuf, address: SocketAddr, runner: &Path) -> HubConfig {
    HubConfig {
        listen: "127.0.0.1:0".parse().expect("listen address"),
        public_address: format!("https://{address}"),
        data_dir,
        max_concurrent_per_account: 1,
        runner_program: "node".into(),
        runner_args: vec![runner.to_string_lossy().into_owned()],
        limits: Limits::default(),
        heartbeat_timeout: Duration::from_secs(30),
        default_egress: Vec::new(),
        retention: RetentionConfig::default(),
    }
}

async fn enroll(service: &HubService, name: &str) -> tagteam_node::NodeIdentity {
    let invite = match service
        .handle(LocalRequest::Invite {
            name: name.into(),
            ttl_seconds: 120,
        })
        .await
        .expect("node invite")
    {
        LocalResponseBody::Invited(invite) => invite.invite,
        other => panic!("unexpected invite response: {other:?}"),
    };
    let identity = NodeEnrollment::enroll(
        &invite,
        name,
        tagteam_proto::v1::Platform {
            os: tagteam_proto::v1::Os::Macos as i32,
            arch: tagteam_proto::v1::Arch::Arm64 as i32,
            os_version: "loopback recovery".into(),
        },
    )
    .await
    .expect("node enrollment");
    service
        .handle(LocalRequest::Approve { name: name.into() })
        .await
        .expect("node approval");
    identity
}

async fn run_node(
    identity: tagteam_node::NodeIdentity,
    data: PathBuf,
) -> (CommandJournal, tokio::task::JoinHandle<()>) {
    fs::create_dir_all(&data).expect("node data directory");
    let journal = CommandJournal::open(data.join("commands.json")).expect("command journal");
    let source = HubBundleSource::connect(&identity)
        .await
        .expect("bundle source");
    let runner = CommandRunner::new(&data, journal.clone(), source)
        .with_executable(env!("CARGO_BIN_EXE_tagteam-node"));
    let session = NodeSession::new(identity, RecoveryHeartbeat)
        .pending_source(journal.clone())
        .start();
    let sender = session.sender();
    let mut events = session;
    let event_loop = tokio::spawn(async move {
        while let Some(event) = events.next_event().await {
            match event {
                SessionEvent::Command(command) => runner.dispatch(*command, sender.clone()),
                SessionEvent::Cancel(cancel) => runner.cancel(&cancel.command_id),
                SessionEvent::Drain(_) | SessionEvent::Terminated(_) => break,
                SessionEvent::Welcome(_) | SessionEvent::Reconnecting { .. } => {}
            }
        }
    });
    (journal, event_loop)
}

async fn wait_online(service: &HubService, node: tagteam_core::NodeId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !service.nodes().is_online(node) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("node must come online");
}

async fn wait_offline(service: &HubService, node: tagteam_core::NodeId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while service.nodes().is_online(node) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stopped node must leave the online set");
}

async fn status(service: &HubService, subtask: tagteam_core::SubtaskId) -> String {
    let response = service
        .handle(LocalRequest::Status {
            claude_session: None,
            ids: Some(vec![subtask]),
        })
        .await
        .expect("subtask status");
    let LocalResponseBody::Status(status) = response else {
        panic!("unexpected status response: {response:?}");
    };
    status.subtasks[0].state.clone()
}

#[tokio::test]
async fn lost_node_work_is_reassigned_and_completed_by_the_surviving_node() {
    if cfg!(target_os = "linux")
        && (!Path::new("/usr/bin/systemd-run").exists() || !Path::new("/usr/bin/bwrap").exists())
    {
        eprintln!("sandbox tools unavailable; two-node recovery proof is skipped");
        return;
    }

    let root = tempfile::Builder::new()
        .prefix("tt-recovery-")
        .tempdir_in(if Path::new("/tmp").is_dir() {
            "/tmp"
        } else {
            "."
        })
        .expect("short temporary directory");
    let repository = make_repository(root.path());
    let owner_head = git(&repository, &["rev-parse", "HEAD"]);
    let owner_index = git(&repository, &["ls-files", "-s"]);
    let owner_worktree = git(&repository, &["status", "--porcelain=v1", "-z"]);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("hub listener");
    let address = listener.local_addr().expect("hub address");
    let fake_runner =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runner/test-fixtures/fake-runner.mjs");
    let ca = Arc::new(HubCa::generate().expect("hub CA"));
    let service = HubService::open(
        &recovery_config(root.path().join("hub-data"), address, &fake_runner),
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

    let identity_a = enroll(&service, "node-a").await;
    let identity_b = enroll(&service, "node-b").await;
    let node_a_id = identity_a.node_id;
    let node_b_id = identity_b.node_id;
    let node_a_data = root.path().join("node-a-data");
    let node_b_data = root.path().join("node-b-data");
    let late_identity_a = duplicate_identity(&identity_a);
    let (node_a_journal, node_a_loop) = run_node(identity_a, node_a_data.clone()).await;
    let (_node_b_journal, node_b_loop) = run_node(identity_b, node_b_data.clone()).await;
    wait_online(&service, node_a_id).await;
    wait_online(&service, node_b_id).await;

    let dispatch = match service
        .handle(LocalRequest::Dispatch(DispatchInput {
            main_session: MainSessionId::new(),
            claude_session: Some("two-node-recovery-session".into()),
            repo: repository.clone(),
            subtasks: vec![SubtaskSpec {
                title: "Recover from a lost execution node".into(),
                prompt: "recovery: exercise node lease recovery and preserve the runner session"
                    .into(),
                done_when: vec!["test -f result.txt".into()],
                repo: repository.clone(),
                base_ref: "main".into(),
                include_uncommitted: false,
                needs: Needs::default(),
                limits: Limits {
                    command_timeout: Duration::from_secs(60),
                    wall_clock: Duration::from_secs(120),
                    ..Limits::default()
                },
                egress: Vec::new(),
                node_hint: Some("node-a".into()),
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
    assert!(matches!(dispatch.subtasks[0].1, Placement::Placed(id) if id == node_a_id));

    let workspace_a = node_a_data
        .join("workspaces")
        .join(subtask.to_string())
        .join("repo");
    tokio::time::timeout(Duration::from_secs(25), async {
        while !workspace_a.join("node-a-started").is_file() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("node A must reach its slow sandbox command");
    let initial = Journal::open(service.paths().journal.as_path())
        .expect("hub journal")
        .load_subtask(subtask)
        .expect("load task")
        .expect("task exists");
    assert_eq!(initial.node, Some(node_a_id));
    let runner_session = initial
        .runner_session
        .expect("A established a runner session");

    node_a_loop.abort();
    let _ = node_a_loop.await;
    wait_offline(&service, node_a_id).await;
    let results_before: std::collections::HashSet<String> = node_a_journal
        .pending()
        .expect("node A command results")
        .into_iter()
        .map(|ack| ack.command_id)
        .collect();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let recovered = loop {
        service.reconcile_nodes().await.expect("reconcile leases");
        let task = Journal::open(service.paths().journal.as_path())
            .expect("hub journal")
            .load_subtask(subtask)
            .expect("load task")
            .expect("task exists");
        if task.node == Some(node_b_id)
            && matches!(
                task.state,
                SubtaskState::Preparing | SubtaskState::Running | SubtaskState::Done
            )
        {
            break task;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "node A lease did not reassign to B: state={:?} node={:?}",
            task.state,
            task.node
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        recovered.runner_session.as_deref(),
        Some(runner_session.as_str()),
        "reassignment must resume the original runner session"
    );

    let done_deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let state = status(&service, subtask).await;
        if state == "done" {
            break;
        }
        if state == "failed" {
            let task = Journal::open(service.paths().journal.as_path())
                .expect("hub journal")
                .load_subtask(subtask)
                .expect("load task")
                .expect("task exists");
            panic!("recovered subtask failed: {task:?}");
        }
        assert!(
            tokio::time::Instant::now() < done_deadline,
            "recovered subtask did not reach done; last state={state}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let collected = match service
        .handle(LocalRequest::Collect {
            claude_session: None,
            subtask,
            worktree: false,
        })
        .await
        .expect("collect recovered result")
    {
        LocalResponseBody::Collected(collected) => collected,
        other => panic!("unexpected collect response: {other:?}"),
    };
    assert!(collected.result.summary.contains("node B"));
    let branch = collected.result.branch.as_deref().expect("review branch");
    assert_eq!(
        git(&repository, &["show", &format!("{branch}:result.txt")]),
        "written by node B\n"
    );
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), owner_head);
    assert_eq!(git(&repository, &["ls-files", "-s"]), owner_index);
    assert_eq!(
        git(&repository, &["status", "--porcelain=v1", "-z"]),
        owner_worktree
    );
    assert!(!repository.join("result.txt").exists());
    let branches = git(
        &repository,
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    );
    assert_eq!(branches.lines().count(), 2, "main plus one review branch");

    // A's cancelled sandbox command may still complete locally and become a
    // durable result. Reconnecting the same identity must not replace B's
    // completed task or duplicate the review branch.
    tokio::time::timeout(Duration::from_secs(20), async {
        while !workspace_a.join("result.txt").is_file() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("node A's delayed sandbox side effect completes only in its workspace");
    let late_ack = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ack) = node_a_journal
                .pending()
                .expect("node A command results")
                .into_iter()
                .find(|ack| !results_before.contains(&ack.command_id))
            {
                break ack;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("node A durably records the late command result");
    assert!(matches!(
        late_ack.result.as_ref().and_then(|result| result.result.as_ref()),
        Some(tagteam_proto::v1::command_result::Result::Exec(result)) if result.exit_code == 0
    ));

    let (late_journal, late_node_loop) = run_node(late_identity_a, node_a_data.clone()).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !service.nodes().is_online(node_a_id) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("node A reconnects and replays its late result");
    assert!(
        late_journal
            .pending()
            .expect("reopened node A journal")
            .iter()
            .any(|ack| ack.command_id == late_ack.command_id),
        "the late result must be present in the reconnect Hello"
    );
    let after_late = Journal::open(service.paths().journal.as_path())
        .expect("hub journal")
        .load_subtask(subtask)
        .expect("load task")
        .expect("task exists");
    assert_eq!(after_late.state, SubtaskState::Done);
    assert_eq!(after_late.node, Some(node_b_id));
    assert_eq!(
        after_late.runner_session.as_deref(),
        Some(runner_session.as_str())
    );
    assert_eq!(
        git(&repository, &["show", &format!("{branch}:result.txt")]),
        "written by node B\n"
    );
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), owner_head);
    assert_eq!(git(&repository, &["ls-files", "-s"]), owner_index);
    assert_eq!(
        git(&repository, &["status", "--porcelain=v1", "-z"]),
        owner_worktree
    );

    late_node_loop.abort();
    node_b_loop.abort();
    let _ = shutdown.send(());
    let _ = server.await;
}
