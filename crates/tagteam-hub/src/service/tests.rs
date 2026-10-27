//! Dispatch-boundary tests for repository snapshots, validation, and placement.
//!
//! These tests exercise the application service directly. They do not start a
//! node transport or an Agent SDK runner, so successful placement is not proof
//! of remote execution.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    time::Duration,
};

use semver::VersionReq;
use tagteam_core::{
    Bytes, Capacity, Dispatch, FailReason, Limits, MainSessionId, Needs, Node, NodeId, NodePolicy,
    NodeState, OnReturn, Os, OwnerPresence, Platform, Question, QuestionId, ReassignReason,
    Subtask, SubtaskId, SubtaskSpec, SubtaskState, Thermal, Timestamp, Trust,
    local_api::{DispatchInput, DispatchReply, LocalRequest, LocalResponseBody},
    runner_proto::HubToRunnerMessage,
    schedule::{NoFitReason, Placement},
    state::SubtaskInput,
};
use tagteam_pki::HubCa;
use tempfile::TempDir;

use crate::{blobs::BlobStore, config::HubConfig};

use super::{
    HubService, branch_name, combined_egress, ensure_prepare_succeeded, normalize_and_validate_spec,
};

fn config(directory: &TempDir) -> HubConfig {
    HubConfig {
        listen: "127.0.0.1:0".parse().expect("listen address"),
        public_address: "https://127.0.0.1:7443".into(),
        data_dir: directory.path().join("hub-data"),
        max_concurrent_per_account: 2,
        runner_program: "node".into(),
        runner_args: Vec::new(),
        limits: Limits::default(),
        heartbeat_timeout: Duration::from_secs(90),
        default_egress: Vec::new(),
        retention: crate::config::RetentionConfig::default(),
    }
}

async fn service(directory: &TempDir) -> HubService {
    let ca = Arc::new(HubCa::generate().expect("CA"));
    HubService::open(&config(directory), ca)
        .await
        .expect("service")
}

#[tokio::test]
async fn startup_and_periodic_retention_remove_old_orphaned_blobs() {
    let directory = TempDir::new().expect("directory");
    let config = config(&directory);
    let blob_root = config.data_dir.join("blobs");
    let store = BlobStore::open(&blob_root).expect("blob store");
    let startup_blob = store.put(b"startup orphan").expect("startup blob");
    let startup_path = blob_path(&blob_root, &startup_blob);
    set_modified_to_epoch(&startup_path);

    let ca = Arc::new(HubCa::generate().expect("CA"));
    let service = HubService::open(&config, ca).await.expect("service opens");
    assert!(!startup_path.exists(), "open performs the startup sweep");

    let periodic_blob = store.put(b"periodic orphan").expect("periodic blob");
    let periodic_path = blob_path(&blob_root, &periodic_blob);
    set_modified_to_epoch(&periodic_path);
    let counts = service.run_retention().await.expect("periodic sweep");
    assert_eq!(counts.blobs, 1);
    assert!(!periodic_path.exists());
}

#[test]
fn workspace_egress_combines_defaults_and_subtask_hosts_once() {
    let defaults = ["crates.io", "REGISTRY.NPMJS.ORG"]
        .map(str::to_owned)
        .to_vec();
    let extra = ["CRATES.IO.", "git.example", "git.example"]
        .map(str::to_owned)
        .to_vec();
    assert_eq!(
        combined_egress(&defaults, &extra).expect("egress"),
        ["crates.io", "registry.npmjs.org", "git.example"]
    );
    assert!(combined_egress(&defaults, &["https://bad.example".into()]).is_err());
}

fn blob_path(root: &Path, reference: &tagteam_core::BlobRef) -> PathBuf {
    let digest = reference.digest_hex();
    root.join(&digest[..2]).join(digest)
}

fn set_modified_to_epoch(path: &Path) {
    let file = fs::File::options()
        .read(true)
        .write(true)
        .open(path)
        .expect("open blob for timestamp update");
    file.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .expect("set old modification time");
}

fn run_git(repository: &Path, args: &[&str]) -> Output {
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
    output
}

fn init_repository(directory: &TempDir) -> (PathBuf, String) {
    let repository = directory.path().join("repository");
    fs::create_dir_all(&repository).expect("repository directory");
    run_git(&repository, &["init", "-q"]);
    run_git(&repository, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    run_git(
        &repository,
        &["config", "user.name", "Tag Team Service Tests"],
    );
    run_git(
        &repository,
        &["config", "user.email", "tagteam-service@example.invalid"],
    );
    fs::write(repository.join("tracked.txt"), "base\n").expect("base file");
    run_git(&repository, &["add", "-A"]);
    run_git(&repository, &["commit", "-qm", "base"]);
    let base = String::from_utf8_lossy(&run_git(&repository, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_owned();
    (repository, base)
}

fn spec(repository: &Path, title: &str, include_uncommitted: bool) -> SubtaskSpec {
    SubtaskSpec {
        title: title.into(),
        prompt: "Implement the requested change.".into(),
        done_when: vec!["cargo test".into()],
        repo: repository.to_path_buf(),
        base_ref: "main".into(),
        include_uncommitted,
        needs: Needs::default(),
        limits: Limits::default(),
        egress: Vec::new(),
        node_hint: None,
    }
}

fn active_node() -> Node {
    Node {
        id: NodeId::new(),
        name: "test-node".into(),
        platform: Platform {
            os: Os::MacOs,
            arch: tagteam_core::Arch::Arm64,
            os_version: "test".into(),
        },
        state: NodeState::Active,
        cert_sha256: [7; 32],
        capacity: Capacity {
            cores_total: 8,
            cores_free: 8.0,
            memory_free: Bytes::gib(16),
            disk_free: Bytes::gib(50),
            running: 0,
            on_battery: false,
            battery_known: true,
            local_minute_of_day: Some(10 * 60),
            thermal: Thermal::Nominal,
        },
        toolchains: Vec::new(),
        repo_heads: Vec::new(),
        policy: NodePolicy {
            max_subtasks: 4,
            ..NodePolicy::default()
        },
        presence: OwnerPresence::Away {
            idle_for: Duration::from_secs(600),
        },
        last_seen: Timestamp::now(),
        trust: Trust::Own,
    }
}

async fn dispatch(
    service: &HubService,
    repository: &Path,
    subtasks: Vec<SubtaskSpec>,
) -> DispatchReply {
    let response = service
        .handle(LocalRequest::Dispatch(DispatchInput {
            main_session: MainSessionId::new(),
            claude_session: Some("claude-session".into()),
            repo: repository.to_path_buf(),
            subtasks,
            spread: false,
        }))
        .await
        .expect("dispatch");
    let LocalResponseBody::Dispatched(reply) = response else {
        panic!("unexpected dispatch response: {response:?}");
    };
    reply
}

async fn load_subtask(service: &HubService, id: SubtaskId) -> tagteam_core::Subtask {
    service
        .journal(move |journal| journal.load_subtask(id))
        .await
        .expect("load subtask")
        .expect("subtask exists")
}

async fn seed_running_task(
    service: &HubService,
    node: NodeId,
    claude_session: &str,
    title: &str,
) -> Subtask {
    let created = Timestamp::now();
    let subtask = Subtask {
        id: SubtaskId::new(),
        dispatch: tagteam_core::DispatchId::new(),
        spec: spec(Path::new("/workspace/repo"), title, false),
        state: SubtaskState::Running,
        node: Some(node),
        attempt: 0,
        base: tagteam_core::GitOid::zero(),
        repo_id: tagteam_core::RepoId(tagteam_core::GitOid::zero()),
        branch: format!(
            "tagteam/{}-{}",
            title.to_ascii_lowercase().replace(' ', "-"),
            SubtaskId::new()
        ),
        runner_session: Some("owner-return-session".into()),
        created,
        started: Some(created),
        finished: None,
        usage: tagteam_core::Usage::default(),
    };
    let dispatch = Dispatch {
        id: subtask.dispatch,
        main_session: MainSessionId::new(),
        claude_session: Some(claude_session.into()),
        repo: PathBuf::from("/workspace/repo"),
        created,
    };
    let stored = subtask.clone();
    service
        .journal(move |journal| {
            journal.create_dispatch(&dispatch)?;
            // create_subtask accepts the Running state as a durable recovery
            // fixture; production always reaches it through the state machine.
            journal.create_subtask(&stored)
        })
        .await
        .expect("seed running task");
    subtask
}

#[tokio::test]
async fn dispatch_snapshots_dirty_and_untracked_work_without_secrets() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, base) = init_repository(&directory);
    let service = service(&directory).await;
    let node = active_node();
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    fs::write(repository.join("tracked.txt"), "changed\n").expect("dirty tracked file");
    fs::write(repository.join("new.txt"), "untracked\n").expect("untracked file");
    fs::write(repository.join(".env"), "do not dispatch\n").expect("secret file");

    let reply = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Dirty: work / edge cases!", true)],
    )
    .await;
    assert_eq!(reply.excluded_paths, [".env"]);
    assert_eq!(reply.subtasks.len(), 1);
    assert_eq!(reply.subtasks[0].1, Placement::Placed(node_id));
    assert!(reply.watch_command.contains(&reply.dispatch.to_string()));

    let subtask_id = reply.subtasks[0].0;
    let task = load_subtask(&service, subtask_id).await;
    assert_eq!(task.state, SubtaskState::Preparing);
    assert_eq!(task.node, Some(node_id));
    assert_ne!(task.base.0, base, "the task points at the snapshot commit");
    assert!(task.branch.starts_with("tagteam/dirty-work-edge-cases-"));
    assert_eq!(
        task.spec.repo,
        repository.canonicalize().expect("canonical repo")
    );

    let tracked = run_git(
        &repository,
        &["show", &format!("{}:tracked.txt", task.base.0)],
    );
    assert_eq!(tracked.stdout, b"changed\n");
    let untracked = run_git(&repository, &["show", &format!("{}:new.txt", task.base.0)]);
    assert_eq!(untracked.stdout, b"untracked\n");
    let secret = Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["cat-file", "-e", &format!("{}:.env", task.base.0)])
        .output()
        .expect("inspect secret absence");
    assert!(
        !secret.status.success(),
        "secret must not enter the snapshot"
    );
}

#[tokio::test]
async fn dispatch_without_nodes_is_queued_with_an_explicit_reason() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let service = service(&directory).await;

    let reply = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "No node yet", false)],
    )
    .await;
    assert_eq!(
        reply.subtasks[0].1,
        Placement::Queued(vec![NoFitReason::NoNodes])
    );
    assert!(reply.excluded_paths.is_empty());
    let task = load_subtask(&service, reply.subtasks[0].0).await;
    assert_eq!(task.state, SubtaskState::Queued);
    assert_eq!(task.node, None);
}

#[tokio::test]
async fn owner_return_policies_preserve_pause_and_reassign_hand_back_separately() {
    let directory = tempfile::tempdir().expect("tempdir");
    let service = service(&directory).await;
    let node = active_node();
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let paused_task = seed_running_task(&service, node_id, "pause-session", "Owner pause").await;
    service
        .apply_owner_return_policy(node_id, OnReturn::Pause)
        .await
        .expect("pause policy");
    let paused = load_subtask(&service, paused_task.id).await;
    assert_eq!(paused.state, SubtaskState::Paused);
    assert_eq!(paused.runner_session, paused_task.runner_session);
    let cleanup_id = format!("cleanup-{}-a{}", paused_task.id, paused_task.attempt);
    let pause_cleanup = service
        .journal({
            let cleanup_id = cleanup_id.clone();
            move |journal| journal.load_command(&cleanup_id)
        })
        .await
        .expect("load pause cleanup");
    assert!(
        pause_cleanup.is_none(),
        "pause must retain the borrowed workspace"
    );

    service
        .apply_owner_return_policy(node_id, OnReturn::HandBack)
        .await
        .expect("hand-back policy");
    let handed_back = load_subtask(&service, paused_task.id).await;
    assert_eq!(
        handed_back.state,
        SubtaskState::Reassigning {
            reason: ReassignReason::OwnerReturned
        }
    );
    let hand_back_cleanup = service
        .journal(move |journal| journal.load_command(&cleanup_id))
        .await
        .expect("load hand-back cleanup");
    assert!(
        hand_back_cleanup.is_some(),
        "hand-back must durably request workspace cleanup"
    );

    let running_task = seed_running_task(&service, node_id, "finish-session", "Owner finish").await;
    service
        .apply_owner_return_policy(node_id, OnReturn::FinishRunning)
        .await
        .expect("finish policy");
    assert_eq!(
        load_subtask(&service, running_task.id).await.state,
        SubtaskState::Running
    );
    let finish_cleanup_id = format!("cleanup-{}-a{}", running_task.id, running_task.attempt);
    let finish_cleanup = service
        .journal(move |journal| journal.load_command(&finish_cleanup_id))
        .await
        .expect("load finish cleanup");
    assert!(
        finish_cleanup.is_none(),
        "finish_running must leave the active attempt untouched"
    );
}

#[tokio::test]
async fn hub_restart_recovers_a_placing_subtask_instead_of_stranding_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, base) = init_repository(&directory);
    let config = config(&directory);
    let ca = Arc::new(HubCa::generate().expect("CA"));
    let service = HubService::open(&config, Arc::clone(&ca))
        .await
        .expect("service");

    let dispatch_id = tagteam_core::DispatchId::new();
    let created = Timestamp::now();
    let dispatch = Dispatch {
        id: dispatch_id,
        main_session: MainSessionId::new(),
        claude_session: Some("placing-recovery-session".into()),
        repo: repository.canonicalize().expect("canonical repo"),
        created,
    };
    service
        .journal(move |journal| journal.create_dispatch(&dispatch))
        .await
        .expect("dispatch");

    let subtask = Subtask {
        id: SubtaskId::new(),
        dispatch: dispatch_id,
        spec: spec(&repository, "Crash between create and placement", false),
        state: SubtaskState::Placing,
        node: None,
        attempt: 0,
        base: tagteam_core::GitOid(base.clone()),
        repo_id: tagteam_core::RepoId(tagteam_core::GitOid(base)),
        branch: "tagteam/placing-recovery".into(),
        runner_session: None,
        created,
        started: None,
        finished: None,
        usage: tagteam_core::Usage::default(),
    };
    let subtask_id = subtask.id;
    service
        .journal(move |journal| journal.create_subtask(&subtask))
        .await
        .expect("placing subtask");
    drop(service);

    let restarted = HubService::open(&config, ca)
        .await
        .expect("restarted service");
    restarted
        .schedule_pending_work()
        .await
        .expect("recover placement");
    let recovered = load_subtask(&restarted, subtask_id).await;
    assert_eq!(recovered.state, SubtaskState::Queued);
    assert_eq!(recovered.node, None);
}

#[tokio::test]
async fn account_capacity_is_enforced_across_dispatches() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let mut config = config(&directory);
    config.max_concurrent_per_account = 1;
    let ca = Arc::new(HubCa::generate().expect("CA"));
    let service = HubService::open(&config, ca).await.expect("service");
    let mut node = active_node();
    node.policy.max_subtasks = 1;
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let first = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "First worker", false)],
    )
    .await;
    assert_eq!(first.subtasks[0].1, Placement::Placed(node_id));

    let second = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Second worker", false)],
    )
    .await;
    assert_eq!(
        second.subtasks[0].1,
        Placement::Queued(vec![NoFitReason::AccountAtCapacity])
    );

    let third = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Third worker", false)],
    )
    .await;
    assert_eq!(
        third.subtasks[0].1,
        Placement::Queued(vec![NoFitReason::AccountAtCapacity])
    );
}

#[tokio::test]
async fn node_subtask_limit_is_enforced_across_dispatches() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let mut config = config(&directory);
    config.max_concurrent_per_account = 4;
    let ca = Arc::new(HubCa::generate().expect("CA"));
    let service = HubService::open(&config, ca).await.expect("service");
    let mut node = active_node();
    node.policy.max_subtasks = 1;
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let first = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Node slot", false)],
    )
    .await;
    assert_eq!(first.subtasks[0].1, Placement::Placed(node_id));

    let second = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "No node slot", false)],
    )
    .await;
    assert_eq!(
        second.subtasks[0].1,
        Placement::Queued(vec![NoFitReason::NodeAtCapacity { node: node_id }])
    );
}

#[tokio::test]
async fn source_bundle_is_verified_and_content_addressed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let service = service(&directory).await;
    let reply = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Bundle me", false)],
    )
    .await;
    let task = load_subtask(&service, reply.subtasks[0].0).await;
    let blob = service
        .create_source_bundle(&task)
        .await
        .expect("source bundle is stored");
    assert!(blob.size > 0);

    let bytes = tokio::task::spawn_blocking({
        let root = service.paths().blobs.clone();
        move || BlobStore::open(root).and_then(|store| store.read(&blob))
    })
    .await
    .expect("blob worker")
    .expect("blob reads back");
    let bundle_path = directory.path().join("verified.bundle");
    fs::write(&bundle_path, &bytes).expect("bundle file");
    tagteam_git::Repository::open(&repository)
        .expect("repository")
        .verify_bundle(&bundle_path)
        .expect("bundle connectivity");
}

#[tokio::test]
async fn an_offline_node_does_not_fail_its_durable_placement() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let service = service(&directory).await;
    let node = active_node();
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let reply = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Prepared later", false)],
    )
    .await;
    assert_eq!(reply.subtasks[0].1, Placement::Placed(node_id));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let task = load_subtask(&service, reply.subtasks[0].0).await;
    assert_eq!(task.state, SubtaskState::Preparing);
    assert_eq!(task.node, Some(node_id));
}

#[tokio::test]
async fn an_unavailable_node_fails_preparation_after_its_wall_clock_budget() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let service = service(&directory).await;
    let node = active_node();
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let mut task = spec(&repository, "Preparation budget", false);
    task.limits.wall_clock = Duration::from_millis(1);
    task.limits.command_timeout = Duration::from_secs(1);
    let reply = dispatch(&service, &repository, vec![task]).await;
    assert_eq!(reply.subtasks[0].1, Placement::Placed(node_id));
    let subtask_id = reply.subtasks[0].0;

    let mut state = None;
    // Bundle creation and driver startup can exceed a one-millisecond task
    // budget before the first retry check; leave enough wall time for the
    // asynchronous failure transition without weakening the assertion.
    for _ in 0..300 {
        let task = load_subtask(&service, subtask_id).await;
        if matches!(task.state, SubtaskState::Failed { .. }) {
            state = Some(task.state);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let Some(SubtaskState::Failed {
        reason: FailReason::Bundle(reason),
    }) = state
    else {
        panic!("preparation must fail terminally after its budget: {state:?}");
    };
    assert!(reason.contains("did not acknowledge workspace preparation"));
}

#[tokio::test]
async fn cancellation_while_preparing_never_reaches_running() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let service = service(&directory).await;
    let node = active_node();
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let reply = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Cancel preparation", false)],
    )
    .await;
    assert_eq!(reply.subtasks[0].1, Placement::Placed(node_id));
    let subtask_id = reply.subtasks[0].0;
    service
        .handle(LocalRequest::Cancel {
            subtask: subtask_id,
            reason: "operator cancelled".into(),
            claude_session: None,
        })
        .await
        .expect("cancel preparing task");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        load_subtask(&service, subtask_id).await.state,
        SubtaskState::Cancelled
    );
}

#[test]
fn prepare_command_requires_a_positive_done_result() {
    assert!(
        ensure_prepare_succeeded(&tagteam_proto::v1::CommandResult {
            command_id: "prepare".into(),
            result: Some(tagteam_proto::v1::command_result::Result::Done(
                tagteam_proto::v1::Done { ok: true }
            )),
        })
        .is_ok()
    );
    assert!(
        ensure_prepare_succeeded(&tagteam_proto::v1::CommandResult {
            command_id: "prepare".into(),
            result: Some(tagteam_proto::v1::command_result::Result::Done(
                tagteam_proto::v1::Done { ok: false }
            )),
        })
        .is_err()
    );
    assert!(
        ensure_prepare_succeeded(&tagteam_proto::v1::CommandResult {
            command_id: "prepare".into(),
            result: Some(tagteam_proto::v1::command_result::Result::Error(
                tagteam_proto::v1::Error {
                    code: 0,
                    message: "failed".into(),
                    retryable: false,
                }
            )),
        })
        .is_err()
    );
}

#[test]
fn runner_tool_calls_map_to_bounded_node_commands() {
    let directory = tempfile::tempdir().expect("tempdir");
    let repository = directory.path().join("repo");
    fs::create_dir_all(&repository).expect("repo directory");
    let task = tagteam_core::Subtask {
        id: SubtaskId::new(),
        dispatch: tagteam_core::DispatchId::new(),
        spec: spec(&repository, "Remote tools", false),
        state: SubtaskState::Running,
        node: Some(NodeId::new()),
        attempt: 0,
        base: tagteam_core::GitOid::zero(),
        repo_id: tagteam_core::RepoId(tagteam_core::GitOid::zero()),
        branch: "tagteam/remote-tools".into(),
        runner_session: None,
        created: Timestamp::now(),
        started: None,
        finished: None,
        usage: tagteam_core::Usage::default(),
    };

    let bash = super::remote_command(
        &task,
        "call-1",
        &tagteam_core::runner_proto::RemoteCall::Bash {
            command: "true".into(),
            timeout_ms: Some(u64::MAX),
        },
    )
    .expect("bash maps");
    assert!(matches!(
        bash.op,
        Some(tagteam_proto::v1::command::Op::Exec(ref exec))
            if exec.argv == ["bash", "-lc", "true"]
                && exec.timeout_ms == u32::try_from(task.spec.limits.command_timeout.as_millis()).unwrap_or(u32::MAX)
    ));

    let read = super::remote_command(
        &task,
        "call-2",
        &tagteam_core::runner_proto::RemoteCall::Read {
            path: "src/lib.rs".into(),
            offset: Some(4),
            limit: Some(8),
        },
    )
    .expect("read maps");
    assert!(matches!(
        read.op,
        Some(tagteam_proto::v1::command::Op::Read(ref read))
            if read.files.len() == 1 && read.files[0].offset == 4 && read.files[0].limit == 8
    ));

    let write = super::remote_command(
        &task,
        "call-3",
        &tagteam_core::runner_proto::RemoteCall::Write {
            path: "result.txt".into(),
            content: "result\n".into(),
        },
    )
    .expect("write maps");
    assert!(matches!(
        write.op,
        Some(tagteam_proto::v1::command::Op::Write(ref write))
            if write.files.len() == 1 && write.files[0].data == b"result\n"
    ));

    let edit = super::remote_command(
        &task,
        "call-4",
        &tagteam_core::runner_proto::RemoteCall::Edit {
            path: "result.txt".into(),
            old: "old".into(),
            new: "new".into(),
            replace_all: true,
        },
    )
    .expect("edit maps");
    assert!(matches!(
        edit.op,
        Some(tagteam_proto::v1::command::Op::Edit(ref edit))
            if edit.old == "old" && edit.new == "new" && edit.replace_all
    ));

    let glob = super::remote_command(
        &task,
        "call-5",
        &tagteam_core::runner_proto::RemoteCall::Glob {
            pattern: "*.rs".into(),
            path: Some("src".into()),
        },
    )
    .expect("glob maps");
    assert!(matches!(
        glob.op,
        Some(tagteam_proto::v1::command::Op::Glob(ref glob))
            if glob.pattern == "*.rs" && glob.cwd == "src"
    ));

    let grep = super::remote_command(
        &task,
        "call-6",
        &tagteam_core::runner_proto::RemoteCall::Grep {
            pattern: "needle".into(),
            path: Some("src".into()),
            glob: Some("*.rs".into()),
            ignore_case: true,
            context: Some(u32::MAX),
            limit: Some(u32::MAX),
        },
    )
    .expect("grep maps");
    assert!(matches!(
        grep.op,
        Some(tagteam_proto::v1::command::Op::Grep(ref grep))
            if grep.path == "src" && grep.context == 100 && grep.limit == 10_000
    ));
    assert!(
        super::remote_command(
            &task,
            "call-7",
            &tagteam_core::runner_proto::RemoteCall::Finish {
                outcome: tagteam_core::Outcome::Completed,
                summary: "done".into(),
                assumptions: Vec::new(),
                checks: Vec::new(),
            },
        )
        .is_err()
    );
    assert!(super::allowed_runner_tool("mcp__tagteam__Bash"));
    assert!(!super::allowed_runner_tool("mcp__tagteam__Agent"));
    assert!(super::runner_system_append(&task).contains("cargo test"));
}

#[tokio::test]
async fn dispatch_reports_the_exact_missing_toolchain() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let service = service(&directory).await;
    let node = active_node();
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let mut task_spec = spec(&repository, "Needs Cargo", false);
    task_spec.needs.toolchains = vec![(
        "cargo".into(),
        VersionReq::parse(">=1.80").expect("requirement"),
    )];
    let reply = dispatch(&service, &repository, vec![task_spec]).await;
    assert!(matches!(
        &reply.subtasks[0].1,
        Placement::Queued(reasons)
            if reasons == &[NoFitReason::MissingToolchain {
                node: node_id,
                name: "cargo".into(),
                needed: ">=1.80".into(),
                available: None,
            }]
    ));
    assert_eq!(
        load_subtask(&service, reply.subtasks[0].0).await.state,
        SubtaskState::Queued
    );
}

#[test]
fn branch_names_are_bounded_safe_and_include_the_subtask_suffix() {
    let id = SubtaskId::new();
    let branch = branch_name(
        "Fix: parser / edge cases! With a title that keeps going past the cap",
        id,
    );
    let suffix = id.to_string()[id.to_string().len() - 6..].to_ascii_lowercase();
    assert_eq!(
        branch,
        format!("tagteam/fix-parser-edge-cases-with-a-title-that-{suffix}")
    );
    assert!(branch.len() <= 54);
    assert!(branch.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '/')
    }));
    assert_eq!(
        branch_name("", id),
        format!(
            "tagteam/task-{}",
            &id.to_string()[id.to_string().len() - 6..].to_ascii_lowercase()
        )
    );
    assert!(branch_name("🚀", id).starts_with("tagteam/task-"));
}

#[test]
fn direct_spec_validation_normalizes_fields_and_rejects_partial_inputs() {
    let directory = tempfile::tempdir().expect("tempdir");
    let repository = directory.path().join("repo");
    fs::create_dir_all(&repository).expect("repo directory");
    let mut valid = spec(&repository, "  Normalize me  ", false);
    valid.prompt = "  Do the work  ".into();
    valid.done_when = vec!["  cargo test  ".into()];
    valid.base_ref = "  main  ".into();
    valid.egress = vec!["  api.example.com  ".into()];
    valid.node_hint = Some("  laptop  ".into());
    valid.repo = PathBuf::from("/wrong");
    normalize_and_validate_spec(&mut valid, &repository).expect("valid spec");
    assert_eq!(valid.title, "Normalize me");
    assert_eq!(valid.prompt, "Do the work");
    assert_eq!(valid.done_when, ["cargo test"]);
    assert_eq!(valid.base_ref, "main");
    assert_eq!(valid.egress, ["api.example.com"]);
    assert_eq!(valid.node_hint.as_deref(), Some("laptop"));
    assert_eq!(valid.repo, repository);

    let mut empty_title = valid.clone();
    empty_title.title = "  ".into();
    assert!(normalize_and_validate_spec(&mut empty_title, &repository).is_err());

    let mut empty_prompt = valid.clone();
    empty_prompt.prompt.clear();
    assert!(normalize_and_validate_spec(&mut empty_prompt, &repository).is_err());

    let mut no_checks = valid.clone();
    no_checks.done_when.clear();
    assert!(normalize_and_validate_spec(&mut no_checks, &repository).is_err());

    let mut empty_check = valid.clone();
    empty_check.done_when = vec!["  ".into()];
    assert!(normalize_and_validate_spec(&mut empty_check, &repository).is_err());

    let mut no_base = valid.clone();
    no_base.base_ref.clear();
    assert!(normalize_and_validate_spec(&mut no_base, &repository).is_err());

    let mut empty_egress = valid.clone();
    empty_egress.egress = vec![" ".into()];
    assert!(normalize_and_validate_spec(&mut empty_egress, &repository).is_err());

    let mut empty_tool = valid.clone();
    empty_tool
        .needs
        .toolchains
        .push(("  ".into(), VersionReq::parse(">=1").expect("requirement")));
    assert!(normalize_and_validate_spec(&mut empty_tool, &repository).is_err());

    let mut zero_cores = valid.clone();
    zero_cores.needs.cores = 0;
    assert!(normalize_and_validate_spec(&mut zero_cores, &repository).is_err());

    let mut zero_turns = valid.clone();
    zero_turns.limits.max_turns = 0;
    assert!(normalize_and_validate_spec(&mut zero_turns, &repository).is_err());

    let mut zero_duration = valid.clone();
    zero_duration.limits.wall_clock = Duration::ZERO;
    assert!(normalize_and_validate_spec(&mut zero_duration, &repository).is_err());

    let mut zero_output = valid.clone();
    zero_output.limits.max_output = Bytes(0);
    assert!(normalize_and_validate_spec(&mut zero_output, &repository).is_err());
}

async fn seed_question(
    service: &HubService,
    claude_session: &str,
    deadline: Timestamp,
) -> (
    SubtaskId,
    QuestionId,
    tokio::sync::mpsc::Receiver<HubToRunnerMessage>,
) {
    let dispatch = Dispatch {
        id: tagteam_core::DispatchId::new(),
        main_session: MainSessionId::new(),
        claude_session: Some(claude_session.into()),
        repo: PathBuf::from("/workspace/repo"),
        created: Timestamp::now(),
    };
    let task = Subtask {
        id: SubtaskId::new(),
        dispatch: dispatch.id,
        spec: spec(Path::new("/workspace/repo"), "Interactive task", false),
        state: SubtaskState::Running,
        node: None,
        attempt: 0,
        base: tagteam_core::GitOid::zero(),
        repo_id: tagteam_core::RepoId(tagteam_core::GitOid::zero()),
        branch: "tagteam/interactive-task".into(),
        runner_session: Some("runner-session".into()),
        created: Timestamp::now(),
        started: Some(Timestamp::now()),
        finished: None,
        usage: tagteam_core::Usage::default(),
    };
    let question = Question {
        id: QuestionId::new(),
        subtask: task.id,
        text: "Which compatibility path should I keep?".into(),
        options: vec!["old".into(), "new".into()],
        context: Some("The answer changes the migration surface.".into()),
        deadline,
    };
    let task_id = task.id;
    let question_id = question.id;
    service
        .journal(move |journal| {
            journal.create_dispatch(&dispatch)?;
            journal.create_subtask(&task)?;
            journal.transition(
                task.id,
                SubtaskInput::Ask {
                    question,
                    call_id: "ask-call-1".into(),
                },
                Timestamp::now(),
            )?;
            Ok(())
        })
        .await
        .expect("seed question");
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    service.runners.lock().await.insert(task_id, sender);
    (task_id, question_id, receiver)
}

#[tokio::test]
async fn answer_commits_state_then_delivers_the_pending_tool_result() {
    let directory = tempfile::tempdir().expect("tempdir");
    let service = service(&directory).await;
    let deadline = Timestamp::now().checked_add(Duration::from_secs(60));
    let (subtask, question, mut runner) = seed_question(&service, "claude-a", deadline).await;

    let response = service
        .handle(LocalRequest::Answer {
            subtask,
            question: Some(question),
            text: "Keep the old path for one release.".into(),
            author: tagteam_core::local_api::AnswerAuthor::MainClaude,
            claude_session: Some("claude-a".into()),
        })
        .await
        .expect("answer");
    let LocalResponseBody::Answered(reply) = response else {
        panic!("unexpected answer response: {response:?}");
    };
    assert!(!reply.delivered);
    assert!(!reply.acknowledged);
    assert_eq!(reply.question, question);
    assert_eq!(
        load_subtask(&service, subtask).await.state,
        SubtaskState::Running
    );

    let message = tokio::time::timeout(Duration::from_secs(1), runner.recv())
        .await
        .expect("runner delivery timeout")
        .expect("runner message");
    assert!(matches!(
        message,
        HubToRunnerMessage::Answer {
            question_id,
            call_id,
            text,
            timed_out: false,
            ..
        } if question_id == question
            && call_id == "ask-call-1"
            && text == "Keep the old path for one release."
    ));

    let duplicate = service
        .handle(LocalRequest::Answer {
            subtask,
            question: Some(question),
            text: "A second answer".into(),
            author: tagteam_core::local_api::AnswerAuthor::You,
            claude_session: Some("claude-a".into()),
        })
        .await;
    assert!(duplicate.is_err(), "duplicate answer must be rejected");
}

#[tokio::test]
async fn question_expiry_resumes_the_runner_with_timeout_guidance() {
    let directory = tempfile::tempdir().expect("tempdir");
    let service = service(&directory).await;
    let deadline = Timestamp::now().checked_add(Duration::from_millis(25));
    let (subtask, question, mut runner) = seed_question(&service, "claude-a", deadline).await;
    tokio::time::sleep(Duration::from_millis(40)).await;

    assert_eq!(service.expire_due_questions().await.expect("expire"), 1);
    assert_eq!(
        load_subtask(&service, subtask).await.state,
        SubtaskState::Running
    );
    let message = tokio::time::timeout(Duration::from_secs(1), runner.recv())
        .await
        .expect("runner delivery timeout")
        .expect("runner message");
    assert!(matches!(
        message,
        HubToRunnerMessage::Answer {
            question_id,
            call_id,
            timed_out: true,
            ..
        } if question_id == question && call_id == "ask-call-1"
    ));
    assert_eq!(service.expire_due_questions().await.expect("idempotent"), 0);
}

#[tokio::test]
async fn answer_without_runner_is_durable_replayable_and_cleared_by_acknowledgement() {
    let directory = tempfile::tempdir().expect("tempdir");
    let service = service(&directory).await;
    let deadline = Timestamp::now().checked_add(Duration::from_secs(60));
    let (subtask, question, _old_runner) = seed_question(&service, "claude-a", deadline).await;
    service.runners.lock().await.remove(&subtask);

    let response = service
        .handle(LocalRequest::Answer {
            subtask,
            question: Some(question),
            text: "Keep the adapter.".into(),
            author: tagteam_core::local_api::AnswerAuthor::You,
            claude_session: Some("claude-a".into()),
        })
        .await
        .expect("answer should be durable without a runner");
    let LocalResponseBody::Answered(reply) = response else {
        panic!("unexpected answer response: {response:?}");
    };
    assert!(!reply.delivered);
    assert!(!reply.acknowledged);

    let pending = service
        .journal(move |journal| journal.pending_runner_deliveries(subtask))
        .await
        .expect("pending deliveries");
    assert_eq!(pending.len(), 1);
    let delivery = pending.into_iter().next().expect("one delivery");
    assert_eq!(delivery.id, reply.delivery_id);
    assert!(delivery.sent_at.is_none());
    assert!(matches!(
        &delivery.message,
        HubToRunnerMessage::Answer {
            question_id,
            question_text,
            call_id,
            text,
            timed_out: false,
            ..
        } if *question_id == question
            && question_text == "Which compatibility path should I keep?"
            && call_id == "ask-call-1"
            && text == "Keep the adapter."
    ));

    let (attached_sender, mut attached_receiver) = tokio::sync::mpsc::channel(1);
    service
        .runners
        .lock()
        .await
        .insert(subtask, attached_sender.clone());
    attached_sender
        .send(delivery.message)
        .await
        .expect("replay on runner attach");
    let replayed = attached_receiver.recv().await.expect("replayed delivery");
    assert!(matches!(
        replayed,
        HubToRunnerMessage::Answer { delivery_id, .. } if delivery_id == reply.delivery_id
    ));
    service
        .journal(move |journal| {
            journal.mark_runner_delivery_sent(reply.delivery_id, Timestamp::now())
        })
        .await
        .expect("mark sent");
    service
        .journal(move |journal| {
            journal.acknowledge_runner_delivery(reply.delivery_id, Timestamp::now())
        })
        .await
        .expect("acknowledge");
    service
        .journal(move |journal| {
            journal.acknowledge_runner_delivery(reply.delivery_id, Timestamp::now())
        })
        .await
        .expect("duplicate ack is idempotent");
    assert!(
        service
            .journal(move |journal| journal.pending_runner_deliveries(subtask))
            .await
            .expect("pending after ack")
            .is_empty()
    );
}

#[tokio::test]
async fn steer_without_runner_is_durably_replayed_on_attach() {
    let directory = tempfile::tempdir().expect("tempdir");
    let service = service(&directory).await;
    let deadline = Timestamp::now().checked_add(Duration::from_secs(60));
    let (subtask, _, _runner) = seed_question(&service, "claude-a", deadline).await;
    service.runners.lock().await.remove(&subtask);

    let response = service
        .handle(LocalRequest::Steer {
            subtask,
            text: "Keep the compatibility layer small.".into(),
            claude_session: Some("claude-a".into()),
        })
        .await
        .expect("steer is queued without a runner");
    let LocalResponseBody::Steered {
        delivery_id,
        delivered,
        acknowledged,
        ..
    } = response
    else {
        panic!("unexpected steer response: {response:?}");
    };
    assert!(!delivered);
    assert!(!acknowledged);
    let pending = service
        .journal(move |journal| journal.pending_runner_deliveries(subtask))
        .await
        .expect("pending deliveries");
    assert!(matches!(
        pending.as_slice(),
        [delivery] if delivery.id == delivery_id
            && matches!(&delivery.message, HubToRunnerMessage::Steer { text, .. } if text == "Keep the compatibility layer small.")
    ));
}

#[tokio::test]
async fn expired_question_answer_is_persisted_for_later_runner_replay() {
    let directory = tempfile::tempdir().expect("tempdir");
    let service = service(&directory).await;
    let deadline = Timestamp::now().checked_add(Duration::from_millis(20));
    let (subtask, question, _runner) = seed_question(&service, "claude-a", deadline).await;
    service.runners.lock().await.remove(&subtask);
    tokio::time::sleep(Duration::from_millis(30)).await;

    assert_eq!(service.expire_due_questions().await.expect("expire"), 1);
    let pending = service
        .journal(move |journal| journal.pending_runner_deliveries(subtask))
        .await
        .expect("pending timeout delivery");
    assert!(matches!(
        pending.as_slice(),
        [delivery] if matches!(&delivery.message, HubToRunnerMessage::Answer {
            question_id, timed_out: true, ..
        } if *question_id == question)
    ));
}

#[tokio::test]
async fn session_scope_hides_other_claude_sessions_from_tools_and_events() {
    let directory = tempfile::tempdir().expect("tempdir");
    let service = service(&directory).await;
    let deadline = Timestamp::now().checked_add(Duration::from_secs(60));
    let (first, first_question, _first_runner) =
        seed_question(&service, "session-a", deadline).await;
    let (second, second_question, _second_runner) =
        seed_question(&service, "session-b", deadline).await;

    let status = service
        .handle(LocalRequest::Status {
            ids: None,
            claude_session: Some("session-a".into()),
        })
        .await
        .expect("status");
    let LocalResponseBody::Status(status) = status else {
        panic!("unexpected status response: {status:?}");
    };
    assert_eq!(status.subtasks.len(), 1);
    assert_eq!(status.subtasks[0].id, first);
    assert_eq!(
        status.subtasks[0].open_question_text.as_deref(),
        Some("Which compatibility path should I keep?")
    );

    let events = service
        .handle(LocalRequest::Inbox {
            since_seq: 0,
            wait_ms: 0,
            kinds: None,
            all: false,
            claude_session: Some("session-a".into()),
        })
        .await
        .expect("inbox");
    let LocalResponseBody::Events(events) = events else {
        panic!("unexpected inbox response: {events:?}");
    };
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| event.subtask == first));

    let cross_session_answer = service
        .handle(LocalRequest::Answer {
            subtask: second,
            question: Some(second_question),
            text: "should not cross sessions".into(),
            author: tagteam_core::local_api::AnswerAuthor::You,
            claude_session: Some("session-a".into()),
        })
        .await;
    assert!(cross_session_answer.is_err());

    let own_question = service
        .journal(move |journal| journal.load_question(first_question))
        .await
        .expect("journal")
        .expect("question");
    assert_eq!(own_question.question.subtask, first);
}

#[tokio::test]
async fn an_expired_node_lease_reassigns_its_unfinished_work() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (repository, _) = init_repository(&directory);
    let mut config = config(&directory);
    config.heartbeat_timeout = Duration::from_millis(1);
    let ca = Arc::new(HubCa::generate().expect("CA"));
    let service = HubService::open(&config, ca).await.expect("service");
    let node = active_node();
    let node_id = node.id;
    service
        .journal(move |journal| journal.create_node(&node))
        .await
        .expect("node");

    let reply = dispatch(
        &service,
        &repository,
        vec![spec(&repository, "Survive node loss", false)],
    )
    .await;
    let subtask_id = reply.subtasks[0].0;
    let mut observed = None;
    for _ in 0..100 {
        service.reconcile_nodes().await.expect("reconcile");
        let task = load_subtask(&service, subtask_id).await;
        if matches!(
            task.state,
            SubtaskState::Reassigning {
                reason: tagteam_core::ReassignReason::NodeLost
            }
        ) {
            observed = Some(task);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let task = observed.expect("expired lease must move the task to reassigning");
    assert_eq!(task.node, Some(node_id));
    let node = service
        .journal(move |journal| journal.load_node(node_id))
        .await
        .expect("load node")
        .expect("node");
    assert_eq!(node.state, NodeState::Lost);
}
