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
    Bytes, Capacity, Limits, MainSessionId, Needs, Node, NodeId, NodePolicy, NodeState, Os,
    OwnerPresence, Platform, SubtaskId, SubtaskSpec, SubtaskState, Thermal, Timestamp, Trust,
    local_api::{DispatchInput, DispatchReply, LocalRequest, LocalResponseBody},
    schedule::{NoFitReason, Placement},
};
use tagteam_pki::HubCa;
use tempfile::TempDir;

use crate::config::HubConfig;

use super::{HubService, branch_name, normalize_and_validate_spec};

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
    }
}

async fn service(directory: &TempDir) -> HubService {
    let ca = Arc::new(HubCa::generate().expect("CA"));
    HubService::open(&config(directory), ca)
        .await
        .expect("service")
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
