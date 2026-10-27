use std::path::PathBuf;

use super::*;
use tagteam_core::{
    AnsweredBy, Arch, Capacity, Check, DiffStat, GitOid, Limits, MainSessionId, Needs, NodePolicy,
    Os, Outcome, OwnerPresence, Platform, Question, QuestionId, ReassignReason, RepoId,
    ResultSummary, SubtaskSpec, Trust,
};
use tempfile::tempdir;

fn dispatch() -> Dispatch {
    Dispatch {
        id: tagteam_core::DispatchId::new(),
        main_session: MainSessionId::new(),
        claude_session: None,
        repo: PathBuf::from("/workspace/repo"),
        created: Timestamp(10),
    }
}

fn spec() -> SubtaskSpec {
    SubtaskSpec {
        title: "task".into(),
        prompt: "do it".into(),
        done_when: vec!["done".into()],
        repo: PathBuf::from("/workspace/repo"),
        base_ref: "main".into(),
        include_uncommitted: false,
        needs: Needs::default(),
        limits: Limits::default(),
        egress: vec![],
        node_hint: None,
    }
}

fn subtask(dispatch: &Dispatch) -> Subtask {
    Subtask {
        id: SubtaskId::new(),
        dispatch: dispatch.id,
        spec: spec(),
        state: SubtaskState::Queued,
        node: None,
        attempt: 0,
        base: GitOid("0123456789012345678901234567890123456789".into()),
        repo_id: RepoId(GitOid("1123456789012345678901234567890123456789".into())),
        branch: "tagteam/task".into(),
        runner_session: None,
        created: Timestamp(11),
        started: None,
        finished: None,
        usage: Usage::default(),
    }
}

fn node() -> Node {
    Node {
        id: NodeId::new(),
        name: "node".into(),
        platform: Platform {
            os: Os::MacOs,
            arch: Arch::Arm64,
            os_version: "15.0".into(),
        },
        state: NodeState::Enrolled,
        cert_sha256: [4; 32],
        capacity: Capacity::default(),
        toolchains: vec![],
        repo_heads: vec![],
        policy: NodePolicy::default(),
        presence: OwnerPresence::Present,
        last_seen: Timestamp(1),
        trust: Trust::Own,
    }
}

fn setup() -> (tempfile::TempDir, Journal, Dispatch, Subtask) {
    let dir = tempdir().expect("tempdir");
    let mut journal =
        Journal::open(dir.path().join("nested/journal.sqlite")).expect("open journal");
    let dispatch = dispatch();
    journal.create_dispatch(&dispatch).expect("create dispatch");
    let task = subtask(&dispatch);
    journal.create_subtask(&task).expect("create subtask");
    (dir, journal, dispatch, task)
}

fn setup_running() -> (tempfile::TempDir, Journal, Dispatch, Subtask) {
    let dir = tempdir().expect("tempdir");
    let mut journal =
        Journal::open(dir.path().join("nested/journal.sqlite")).expect("open journal");
    let dispatch = dispatch();
    journal.create_dispatch(&dispatch).expect("create dispatch");
    let mut task = subtask(&dispatch);
    task.state = SubtaskState::Running;
    journal
        .create_subtask(&task)
        .expect("create running subtask");
    (dir, journal, dispatch, task)
}

fn running_question(task: &Subtask) -> Question {
    Question {
        id: QuestionId::new(),
        subtask: task.id,
        text: "Should the migration keep the legacy column?".into(),
        options: vec!["keep".into(), "drop".into()],
        context: Some("The answer changes the rollback plan.".into()),
        deadline: Timestamp(1_000),
    }
}

#[test]
fn migration_is_idempotent_across_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("a/b/journal.sqlite");
    drop(Journal::open(&path).expect("first open"));
    let journal = Journal::open(&path).expect("reopen");
    let version: i64 = journal
        .connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("version");
    let mode: String = journal
        .connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("WAL");
    let foreign_keys: i64 = journal
        .connection
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .expect("foreign keys");
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(mode, "wal");
    assert_eq!(foreign_keys, 1);
}

#[test]
fn transition_commits_state_and_event_together() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let assigned_node = node();
    journal.create_node(&assigned_node).expect("assigned node");
    let result = journal
        .transition(
            task.id,
            SubtaskInput::Placed {
                node: assigned_node.id,
            },
            Timestamp(20),
        )
        .expect("transition");
    assert_eq!(result.state, SubtaskState::Preparing);
    assert_eq!(
        journal.load_subtask_state(task.id).expect("state"),
        Some(SubtaskState::Preparing)
    );
    assert_eq!(
        journal.events(EventFilter::default()).expect("events"),
        vec![result.event]
    );
}

#[test]
fn question_and_answer_round_trip_atomically() {
    let (dir, mut journal, _dispatch, task) = setup_running();
    let question = running_question(&task);

    let asked = journal
        .transition(
            task.id,
            SubtaskInput::Ask {
                question: question.clone(),
                call_id: "tool-call-7".into(),
            },
            Timestamp(100),
        )
        .expect("ask");
    assert_eq!(
        asked.state,
        SubtaskState::WaitingOnMain {
            question: question.id
        }
    );
    assert!(matches!(asked.event.kind, EventKind::Question(_)));

    let stored = journal
        .open_question(task.id)
        .expect("open question")
        .expect("question exists");
    assert_eq!(stored.question, question);
    assert_eq!(stored.call_id, "tool-call-7");
    assert_eq!(stored.answer, None);
    assert_eq!(stored.answered_at, None);

    drop(journal);
    let mut journal = Journal::open(dir.path().join("nested/journal.sqlite")).expect("reopen");
    let answered = journal
        .transition(
            task.id,
            SubtaskInput::Answer {
                question: question.id,
                by: AnsweredBy::MainClaude,
                text: "Keep it for one release.".into(),
            },
            Timestamp(200),
        )
        .expect("answer");
    assert_eq!(answered.state, SubtaskState::Running);
    assert!(matches!(
        answered.event.kind,
        EventKind::Answered {
            question: id,
            by: AnsweredBy::MainClaude,
        } if id == question.id
    ));

    let stored = journal
        .load_question(question.id)
        .expect("load question")
        .expect("stored question");
    assert_eq!(stored.answer.as_deref(), Some("Keep it for one release."));
    assert_eq!(stored.answered_by, Some(AnsweredBy::MainClaude));
    assert_eq!(stored.answered_at, Some(Timestamp(200)));
    assert_eq!(stored.closed_at, None);
    assert!(
        journal
            .open_question(task.id)
            .expect("open question")
            .is_none()
    );
}

#[test]
fn wrong_question_cannot_answer_the_waiting_question() {
    let (_dir, mut journal, _dispatch, task) = setup_running();
    let question = running_question(&task);
    journal
        .transition(
            task.id,
            SubtaskInput::Ask {
                question: question.clone(),
                call_id: "tool-call-7".into(),
            },
            Timestamp(100),
        )
        .expect("ask");

    let rejected = journal.transition(
        task.id,
        SubtaskInput::Answer {
            question: QuestionId::new(),
            by: AnsweredBy::You,
            text: "not for this question".into(),
        },
        Timestamp(200),
    );
    assert!(matches!(rejected, Err(JournalError::IllegalTransition(_))));
    assert_eq!(
        journal.load_subtask_state(task.id).expect("state"),
        Some(SubtaskState::WaitingOnMain {
            question: question.id
        })
    );
    let stored = journal
        .open_question(task.id)
        .expect("open question")
        .expect("still open");
    assert_eq!(stored.answer, None);
}

#[test]
fn timeout_answer_resumes_with_explicit_guidance() {
    let (_dir, mut journal, _dispatch, task) = setup_running();
    let question = running_question(&task);
    journal
        .transition(
            task.id,
            SubtaskInput::Ask {
                question: question.clone(),
                call_id: "tool-call-timeout".into(),
            },
            Timestamp(100),
        )
        .expect("ask");
    let guidance = "No answer. Use the smallest safe assumption.";
    let result = journal
        .transition(
            task.id,
            SubtaskInput::Answer {
                question: question.id,
                by: AnsweredBy::Timeout {
                    guess: guidance.into(),
                },
                text: guidance.into(),
            },
            Timestamp(1_000),
        )
        .expect("timeout answer");
    assert_eq!(result.state, SubtaskState::Running);
    let stored = journal
        .load_question(question.id)
        .expect("question")
        .expect("stored");
    assert_eq!(stored.answer.as_deref(), Some(guidance));
    assert!(matches!(
        stored.answered_by,
        Some(AnsweredBy::Timeout { guess }) if guess == guidance
    ));
}

#[test]
fn cancellation_closes_an_unanswered_question_and_rejects_late_answers() {
    let (_dir, mut journal, _dispatch, task) = setup_running();
    let question = running_question(&task);
    journal
        .transition(
            task.id,
            SubtaskInput::Ask {
                question: question.clone(),
                call_id: "tool-call-cancel".into(),
            },
            Timestamp(100),
        )
        .expect("ask");
    journal
        .transition(
            task.id,
            SubtaskInput::Cancel {
                reason: "owner cancelled".into(),
            },
            Timestamp(110),
        )
        .expect("cancel");
    let stored = journal
        .load_question(question.id)
        .expect("question")
        .expect("stored");
    assert_eq!(stored.closed_at, Some(Timestamp(110)));
    assert_eq!(stored.answer, None);
    assert!(
        journal
            .open_question(task.id)
            .expect("open question")
            .is_none()
    );

    let late = journal.transition(
        task.id,
        SubtaskInput::Answer {
            question: question.id,
            by: AnsweredBy::You,
            text: "too late".into(),
        },
        Timestamp(120),
    );
    assert!(matches!(late, Err(JournalError::IllegalTransition(_))));
}

#[test]
fn reassignment_closes_an_unanswered_question() {
    let (_dir, mut journal, _dispatch, task) = setup_running();
    let question = running_question(&task);
    journal
        .transition(
            task.id,
            SubtaskInput::Ask {
                question: question.clone(),
                call_id: "tool-call-reassign".into(),
            },
            Timestamp(100),
        )
        .expect("ask");
    let reassigned = journal
        .transition(
            task.id,
            SubtaskInput::Reassign {
                from: NodeId::new(),
                reason: ReassignReason::NodeLost,
            },
            Timestamp(120),
        )
        .expect("reassign");
    assert!(matches!(
        reassigned.state,
        SubtaskState::Reassigning {
            reason: ReassignReason::NodeLost
        }
    ));
    let stored = journal
        .load_question(question.id)
        .expect("question")
        .expect("stored");
    assert_eq!(stored.closed_at, Some(Timestamp(120)));
    assert!(
        journal
            .open_question(task.id)
            .expect("open question")
            .is_none()
    );
}

#[test]
fn session_lookups_do_not_leak_across_claude_sessions() {
    let (_dir, mut journal, _dispatch, _task) = setup();
    let mut first = dispatch();
    first.claude_session = Some("session-a".into());
    journal.create_dispatch(&first).expect("first dispatch");
    let first_task = subtask(&first);
    journal.create_subtask(&first_task).expect("first task");

    let mut second = dispatch();
    second.claude_session = Some("session-b".into());
    journal.create_dispatch(&second).expect("second dispatch");
    let second_task = subtask(&second);
    journal.create_subtask(&second_task).expect("second task");

    let visible = journal
        .list_subtasks_for_session("session-a")
        .expect("session tasks");
    assert_eq!(
        visible.iter().map(|task| task.id).collect::<Vec<_>>(),
        [first_task.id]
    );
    assert!(
        journal
            .subtask_belongs_to_session(first_task.id, "session-a")
            .expect("ownership")
    );
    assert!(
        !journal
            .subtask_belongs_to_session(first_task.id, "session-b")
            .expect("ownership")
    );
    assert_eq!(
        journal
            .visible_subtask_ids(Some("session-b"))
            .expect("visible"),
        [second_task.id]
    );
}

#[test]
fn illegal_transition_rolls_back_without_event() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let result = journal.transition(task.id, SubtaskInput::Prepared, Timestamp(20));
    assert!(matches!(result, Err(JournalError::IllegalTransition(_))));
    assert_eq!(
        journal.load_subtask_state(task.id).expect("state"),
        Some(SubtaskState::Queued)
    );
    assert!(
        journal
            .events(EventFilter::default())
            .expect("events")
            .is_empty()
    );
}

#[test]
fn event_sequences_are_monotonic_and_filters_apply() {
    let (dir, mut journal, dispatch, first) = setup();
    let database = dir.path().join("nested/journal.sqlite");
    let assigned_node = node();
    journal.create_node(&assigned_node).expect("assigned node");
    let second = subtask(&dispatch);
    journal.create_subtask(&second).expect("second task");
    let one = journal
        .transition(
            first.id,
            SubtaskInput::Placed {
                node: assigned_node.id,
            },
            Timestamp(20),
        )
        .expect("placed");
    drop(journal);
    let mut journal = Journal::open(database).expect("reopen journal");
    let two = journal
        .transition(first.id, SubtaskInput::Prepared, Timestamp(21))
        .expect("prepared");
    let three = journal
        .transition(
            second.id,
            SubtaskInput::Placed {
                node: assigned_node.id,
            },
            Timestamp(22),
        )
        .expect("placed second");
    assert_eq!((one.event.seq, two.event.seq, three.event.seq), (1, 2, 3));
    let kinds = ["progress"];
    let scoped = journal
        .events(EventFilter {
            since_seq: 1,
            kinds: &kinds,
            scope: EventScope::Subtask(first.id),
            limit: 0,
        })
        .expect("filtered");
    assert_eq!(
        scoped.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![2]
    );
    let watch = [first.id, second.id];
    let watched = journal
        .events(EventFilter {
            since_seq: 2,
            kinds: &[],
            scope: EventScope::Watch(&watch),
            limit: 0,
        })
        .expect("watch");
    assert_eq!(
        watched.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![3]
    );
}

#[test]
fn invites_are_looked_up_by_code_hash() {
    let (_dir, mut journal, _dispatch, _task) = setup();
    journal
        .create_invite("secret-code", "laptop", Timestamp(100))
        .expect("invite");
    assert_eq!(
        journal
            .load_invite("secret-code")
            .expect("lookup")
            .expect("found")
            .name,
        "laptop"
    );
    assert!(journal.load_invite("wrong-code").expect("lookup").is_none());
    let stored: Vec<u8> = journal
        .connection
        .query_row("SELECT code_sha256 FROM invites", [], |row| row.get(0))
        .expect("digest");
    assert_eq!(stored.as_slice(), Sha256::digest(b"secret-code").as_slice());
}

#[test]
fn invite_spend_rejects_an_unknown_code() {
    let (_dir, mut journal, _dispatch, _task) = setup();
    assert!(matches!(
        journal.spend_invite("unknown", Timestamp(10), NodeId::new()),
        Err(JournalError::InvalidInviteCode)
    ));
}

#[test]
fn invite_spend_rejects_expired_codes() {
    let (_dir, mut journal, _dispatch, _task) = setup();
    journal
        .create_invite("expired", "node", Timestamp(10))
        .expect("invite");
    assert!(matches!(
        journal.spend_invite("expired", Timestamp(10), NodeId::new()),
        Err(JournalError::InviteExpired)
    ));
    assert_eq!(
        journal
            .load_invite("expired")
            .expect("load")
            .expect("invite")
            .used_at,
        None
    );
}

#[test]
fn invite_spend_marks_the_code_used_once() {
    let (_dir, mut journal, _dispatch, _task) = setup();
    let node = node();
    journal.create_node(&node).expect("node");
    journal
        .create_invite("one-time", "node", Timestamp(100))
        .expect("invite");
    let spent = journal
        .spend_invite("one-time", Timestamp(50), node.id)
        .expect("spend");
    assert_eq!(spent.used_at, Some(Timestamp(50)));
    assert_eq!(spent.node_id, Some(node.id));
    assert!(matches!(
        journal.spend_invite("one-time", Timestamp(51), node.id),
        Err(JournalError::InviteAlreadyUsed)
    ));
    let loaded = journal
        .load_invite("one-time")
        .expect("load")
        .expect("invite");
    assert_eq!(loaded.used_at, Some(Timestamp(50)));
    assert_eq!(loaded.node_id, Some(node.id));
}

#[test]
fn command_redelivery_and_result_are_idempotent() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let node = node();
    journal.create_node(&node).expect("node");
    let command = Command {
        id: "command-1".into(),
        node_id: node.id,
        subtask_id: task.id,
        operation: serde_json::json!({"exec":"pwd"}),
        state: "pending".into(),
        result: None,
        sent_at: None,
        done_at: None,
    };
    journal.store_command(&command).expect("first command");
    journal.store_command(&command).expect("redelivery");
    assert_eq!(journal.pending_commands(node.id).expect("pending").len(), 1);
    let result = serde_json::json!({"exit_code":0});
    journal
        .store_command_result(&command.id, &result, Timestamp(30))
        .expect("store result");
    journal
        .store_command_result(&command.id, &result, Timestamp(31))
        .expect("repeat result");
    assert!(
        journal
            .pending_commands(node.id)
            .expect("pending")
            .is_empty()
    );
    assert_eq!(
        journal
            .load_command(&command.id)
            .expect("load command")
            .expect("stored")
            .result,
        Some(result)
    );
}

#[test]
fn cancelled_command_is_not_replayed_but_its_result_remains_storable() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let node = node();
    journal.create_node(&node).expect("node");
    let command = Command {
        id: "cancelled-command".into(),
        node_id: node.id,
        subtask_id: task.id,
        operation: serde_json::json!({"exec":"pwd"}),
        state: "pending".into(),
        result: None,
        sent_at: None,
        done_at: None,
    };
    journal.store_command(&command).expect("store command");
    journal.cancel_command(&command.id).expect("cancel command");
    assert!(
        journal
            .pending_commands(node.id)
            .expect("pending")
            .is_empty(),
        "a cancelled command must not be replayed after reconnect"
    );

    let result = serde_json::json!({"exit_code":0});
    journal
        .store_command_result(&command.id, &result, Timestamp(30))
        .expect("a result already produced by the node remains storable");
    let loaded = journal
        .load_command(&command.id)
        .expect("load command")
        .expect("stored command");
    assert_eq!(loaded.state, "complete");
    assert_eq!(loaded.result, Some(result));
}

#[test]
fn subtask_idempotency_compares_every_immutable_creation_field() {
    let (_dir, mut journal, dispatch, task) = setup();
    let assigned_node = node();
    journal.create_node(&assigned_node).expect("assigned node");
    let mut mismatches = Vec::new();

    let mut changed = task.clone();
    changed.dispatch = tagteam_core::DispatchId::new();
    mismatches.push(changed);

    let mut changed = task.clone();
    changed.spec.prompt.push_str(" changed");
    mismatches.push(changed);

    let mut changed = task.clone();
    changed.base.0.push('x');
    mismatches.push(changed);

    let mut changed = task.clone();
    changed.repo_id.0.0.push('x');
    mismatches.push(changed);

    let mut changed = task.clone();
    changed.branch.push_str("-changed");
    mismatches.push(changed);

    let mut changed = task.clone();
    changed.attempt += 1;
    mismatches.push(changed);

    let mut changed = task.clone();
    changed.created = Timestamp(task.created.0 + 1);
    mismatches.push(changed);

    for mismatch in mismatches {
        assert!(matches!(
            journal.create_subtask(&mismatch),
            Err(JournalError::IdempotencyConflict(_))
        ));
    }

    // Mutable state is excluded from the creation identity.
    journal
        .transition(
            task.id,
            SubtaskInput::Placed {
                node: assigned_node.id,
            },
            Timestamp(20),
        )
        .expect("transition");
    journal.create_subtask(&task).expect("same creation retry");
    assert_eq!(dispatch.id, task.dispatch);
}

#[test]
fn node_lifecycle_and_fields_round_trip() {
    let (_dir, mut journal, _dispatch, _task) = setup();
    let node = node();
    journal.create_node(&node).expect("create");
    assert_eq!(
        journal.load_node(node.id).expect("load"),
        Some(node.clone())
    );
    journal.approve_node(node.id).expect("approve");
    assert_eq!(
        journal
            .load_node(node.id)
            .expect("load")
            .expect("present")
            .state,
        NodeState::Active
    );
    journal.pause_node(node.id).expect("pause");
    journal.resume_node(node.id).expect("resume");
    journal.remove_node(node.id, Timestamp(99)).expect("remove");
    let removed: String = journal
        .connection
        .query_row(
            "SELECT state FROM nodes WHERE id=?1",
            [node.id.to_string()],
            |row| row.get(0),
        )
        .expect("state");
    assert_eq!(removed, "removed");
}

#[test]
fn result_summary_and_usage_are_persisted_separately() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let assigned_node = node();
    journal.create_node(&assigned_node).expect("assigned node");
    journal
        .transition(
            task.id,
            SubtaskInput::Placed {
                node: assigned_node.id,
            },
            Timestamp(20),
        )
        .expect("place");
    journal
        .transition(task.id, SubtaskInput::Prepared, Timestamp(21))
        .expect("start runner");
    let usage = Usage {
        input_tokens: 12,
        output_tokens: 8,
        cache_read_tokens: 3,
        turns: 2,
        commands: 1,
    };
    journal.set_usage(task.id, usage).expect("usage");
    let result = ResultSummary {
        outcome: Outcome::Completed,
        summary: "done".into(),
        bundle: Some(tagteam_core::BlobRef {
            sha256: [8; 32],
            size: 64,
        }),
        branch: Some("tagteam/task".into()),
        diffstat: DiffStat {
            files: 1,
            insertions: 2,
            deletions: 0,
        },
        assumptions: vec![],
        checks: vec![Check {
            what: "cargo test".into(),
            command: Some("cargo test".into()),
            passed: true,
            output_tail: String::new(),
        }],
    };
    journal
        .transition(
            task.id,
            SubtaskInput::Finished {
                result: result.clone(),
            },
            Timestamp(22),
        )
        .expect("finish");
    assert_eq!(
        journal.load_subtask_state(task.id).expect("state"),
        Some(SubtaskState::Collecting)
    );
    let stored_usage: String = journal
        .connection
        .query_row(
            "SELECT usage FROM subtasks WHERE id=?1",
            [task.id.to_string()],
            |row| row.get(0),
        )
        .expect("stored usage");
    let stored_summary: String = journal
        .connection
        .query_row(
            "SELECT summary FROM results WHERE subtask_id=?1",
            [task.id.to_string()],
            |row| row.get(0),
        )
        .expect("stored summary");
    assert_eq!(
        serde_json::from_str::<Usage>(&stored_usage).expect("decode usage"),
        usage
    );
    assert_eq!(stored_summary, result.summary);
    assert_eq!(
        journal.load_result_summary(task.id).expect("load result"),
        Some(result.clone())
    );
    let mut landed = result;
    landed.summary = "landed and verified".into();
    journal
        .transition(
            task.id,
            SubtaskInput::Landed {
                result: landed.clone(),
            },
            Timestamp(23),
        )
        .expect("land result");
    assert_eq!(
        journal
            .load_result_summary(task.id)
            .expect("updated result"),
        Some(landed)
    );
    let result_count: i64 = journal
        .connection
        .query_row(
            "SELECT COUNT(*) FROM results WHERE subtask_id=?1",
            [task.id.to_string()],
            |row| row.get(0),
        )
        .expect("result count");
    assert_eq!(result_count, 1);
}

#[test]
fn transition_persists_node_assignment_and_runner_session() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let node = node();
    journal.create_node(&node).expect("node");
    journal
        .transition(
            task.id,
            SubtaskInput::Placed { node: node.id },
            Timestamp(20),
        )
        .expect("place");
    journal
        .transition(
            task.id,
            SubtaskInput::RunnerStarted {
                session: "runner-session-1".into(),
            },
            Timestamp(25),
        )
        .expect("runner started");
    let loaded = journal
        .load_subtask(task.id)
        .expect("load")
        .expect("subtask");
    assert_eq!(loaded.node, Some(node.id));
    assert_eq!(loaded.runner_session.as_deref(), Some("runner-session-1"));
    assert_eq!(loaded.started, Some(Timestamp(25)));
}

#[test]
fn reassignment_placement_increments_attempt_and_emits_destination_event() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let first = node();
    let mut second = node();
    second.name = "second-node".into();
    journal.create_node(&first).expect("first node");
    journal.create_node(&second).expect("second node");
    journal
        .transition(
            task.id,
            SubtaskInput::Placed { node: first.id },
            Timestamp(20),
        )
        .expect("place");
    journal
        .transition(task.id, SubtaskInput::Prepared, Timestamp(21))
        .expect("prepared");
    journal
        .transition(
            task.id,
            SubtaskInput::Reassign {
                from: first.id,
                reason: ReassignReason::NodeLost,
            },
            Timestamp(22),
        )
        .expect("reassign");
    let placed = journal
        .transition(
            task.id,
            SubtaskInput::Placed { node: second.id },
            Timestamp(23),
        )
        .expect("re-place");
    assert_eq!(placed.event.kind, EventKind::ReassignedTo { to: second.id });
    let loaded = journal
        .load_subtask(task.id)
        .expect("load")
        .expect("subtask");
    assert_eq!(loaded.node, Some(second.id));
    assert_eq!(loaded.attempt, 1);
    assert_eq!(loaded.state, SubtaskState::Preparing);
}

#[test]
fn cancelling_a_subtasks_commands_prevents_replay_after_reassignment() {
    let (_dir, mut journal, _dispatch, task) = setup();
    let node = node();
    journal.create_node(&node).expect("node");
    journal
        .store_command(&Command {
            id: "prepare-old-attempt".into(),
            node_id: node.id,
            subtask_id: task.id,
            operation: serde_json::json!({"proto_b64": "old"}),
            state: "pending".into(),
            result: None,
            sent_at: Some(Timestamp(20)),
            done_at: None,
        })
        .expect("store command");
    assert_eq!(journal.pending_commands(node.id).expect("pending").len(), 1);
    assert_eq!(
        journal
            .cancel_subtask_commands(task.id)
            .expect("cancel commands"),
        1
    );
    assert!(
        journal
            .pending_commands(node.id)
            .expect("pending")
            .is_empty()
    );
    assert_eq!(
        journal
            .load_command("prepare-old-attempt")
            .expect("load command")
            .expect("command")
            .state,
        "cancelled"
    );
}

#[test]
fn authenticated_hello_policy_replaces_enrollment_defaults() {
    let (_dir, mut journal, _dispatch, _task) = setup();
    let mut enrolled = node();
    journal.create_node(&enrolled).expect("seed node");
    let policy = NodePolicy {
        only_when_idle: true,
        max_cores: Some(2),
        max_memory: Some(tagteam_core::Bytes(4096)),
        max_subtasks: 2,
        extra_hosts: vec!["git.example".into()],
        ..NodePolicy::default()
    };
    journal
        .update_node_policy(enrolled.id, &policy)
        .expect("policy update");
    enrolled = journal
        .load_node(enrolled.id)
        .expect("load node")
        .expect("node exists");
    assert_eq!(enrolled.policy, policy);
}
