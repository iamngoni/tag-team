//! Application service behind the local daemon API.
//!
//! The service owns the durable journal handle shared by CLI and MCP requests,
//! while node transport remains behind [`crate::nodes::NodePlane`]. Requests are
//! validated at this boundary; callers never receive raw SQLite or filesystem errors.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use data_encoding::BASE32_NOPAD;
use tagteam_core::{
    AnsweredBy, Assumption, BlobRef, Check, Dispatch, DispatchId, Event, EventKind, FailReason,
    MAX_BLOB_BYTES, MainSessionId, NodeState, OnReturn, Outcome, OwnerPresence, Question,
    QuestionId, ReassignReason, RepoId, ResultSummary, Subtask, SubtaskId, SubtaskSpec,
    SubtaskState, Timestamp,
    local_api::{
        AnswerAuthor, AnswerReply, BriefReply, CollectReply, DispatchInput, DispatchReply,
        InviteReply, LOCAL_PROTOCOL_VERSION, LocalRequest, LocalResponseBody, LogsReply, NodeView,
        StatusReply, SubtaskStatus, WatchScope,
    },
    render::{ExecReport, tail_middle},
    runner_proto::{
        HubToRunnerMessage, RemoteCall, RunnerToHubMessage, mcp_timeout_ms, wall_clock_ms,
    },
    schedule::{NoFitReason, Placement, place},
    state::SubtaskInput,
};
use tagteam_git::Repository;
use tagteam_pki::{HubCa, Invite};
use tagteam_proto::v1 as Proto;
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    blobs::BlobStore,
    config::{DataPaths, HubConfig},
    journal::{
        AnswerDeliveryIntent, EventFilter, EventScope, Journal, JournalError, RunnerDelivery,
    },
    nodes::{NodePlane, NodePlaneError},
    runner::{RunnerProcess, RunnerStart},
};

const MAX_INVITE_TTL_SECONDS: u64 = 3_600;
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("configuration failed: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("journal operation failed: {0}")]
    Journal(#[from] JournalError),
    #[error("node plane failed: {0}")]
    NodePlane(#[from] NodePlaneError),
    #[error("blob storage failed: {0}")]
    Blob(#[from] crate::blobs::BlobError),
    #[error("workspace preparation failed: {0}")]
    Preparation(String),
    #[error("runner failed: {0}")]
    Runner(String),
    #[error("request is invalid: {0}")]
    Invalid(String),
    #[error("record was not found: {0}")]
    NotFound(String),
    #[error("Git operation failed: {0}")]
    Git(#[from] tagteam_git::GitError),
    #[error("secure random generation failed")]
    Random,
    #[error("blocking worker stopped")]
    Worker,
    #[error("request is not available yet: {0}")]
    Unsupported(String),
    #[error("internal service error")]
    Internal,
}

#[derive(Debug, Clone)]
struct NodeObservation {
    online: bool,
    presence: OwnerPresence,
    state_since: Timestamp,
}

/// Shared daemon application state.
#[derive(Clone)]
pub struct HubService {
    journal: Arc<Mutex<Journal>>,
    nodes: NodePlane,
    ca: Arc<HubCa>,
    public_address: String,
    paths: DataPaths,
    attachments: Arc<AsyncMutex<HashMap<u32, MainSessionId>>>,
    claude_sessions: Arc<AsyncMutex<HashMap<MainSessionId, String>>>,
    runner_program: String,
    runner_args: Vec<String>,
    heartbeat_timeout: Duration,
    runners: Arc<AsyncMutex<HashMap<SubtaskId, tokio::sync::mpsc::Sender<HubToRunnerMessage>>>>,
    active_commands: Arc<AsyncMutex<HashMap<SubtaskId, (tagteam_core::NodeId, String)>>>,
    node_observations: Arc<AsyncMutex<HashMap<tagteam_core::NodeId, NodeObservation>>>,
    drivers: Arc<AsyncMutex<HashSet<SubtaskId>>>,
    landing: Arc<AsyncMutex<()>>,
    placement: Arc<AsyncMutex<()>>,
    max_concurrent_per_account: usize,
}

impl HubService {
    /// Opens the durable data directory, journal and shared CA-backed node plane.
    pub async fn open(config: &HubConfig, ca: Arc<HubCa>) -> Result<Self, ServiceError> {
        let paths = config.prepare_data_dir()?;
        let journal_path = paths.journal.clone();
        let journal = tokio::task::spawn_blocking(move || Journal::open(journal_path))
            .await
            .map_err(|_| ServiceError::Worker)??;
        let journal = Arc::new(Mutex::new(journal));
        let started = Timestamp::now();
        let initial_nodes = {
            let store = journal.lock().map_err(|_| ServiceError::Internal)?;
            store.list_nodes()?
        };
        let node_observations = initial_nodes
            .into_iter()
            .map(|node| {
                (
                    node.id,
                    NodeObservation {
                        online: false,
                        presence: node.presence,
                        state_since: started,
                    },
                )
            })
            .collect();
        let nodes = NodePlane::with_ca(&paths.journal, &paths.blobs, Arc::clone(&ca)).await?;
        Ok(Self {
            journal,
            nodes,
            ca,
            public_address: config.public_address.clone(),
            paths,
            attachments: Arc::new(AsyncMutex::new(HashMap::new())),
            claude_sessions: Arc::new(AsyncMutex::new(HashMap::new())),
            runner_program: config.runner_program.clone(),
            runner_args: config.runner_args.clone(),
            heartbeat_timeout: config.heartbeat_timeout,
            runners: Arc::new(AsyncMutex::new(HashMap::new())),
            active_commands: Arc::new(AsyncMutex::new(HashMap::new())),
            node_observations: Arc::new(AsyncMutex::new(node_observations)),
            drivers: Arc::new(AsyncMutex::new(HashSet::new())),
            landing: Arc::new(AsyncMutex::new(())),
            placement: Arc::new(AsyncMutex::new(())),
            max_concurrent_per_account: config.max_concurrent_per_account,
        })
    }

    /// Stable data paths for daemon sockets, journal and CA material.
    #[must_use]
    pub fn paths(&self) -> &DataPaths {
        &self.paths
    }

    /// Current hub CA, shared by node transport and invite generation.
    #[must_use]
    pub fn ca(&self) -> &Arc<HubCa> {
        &self.ca
    }

    /// Node plane used by orchestration and device status.
    #[must_use]
    pub fn nodes(&self) -> &NodePlane {
        &self.nodes
    }

    /// Handles one non-streaming local request.
    pub async fn handle(&self, request: LocalRequest) -> Result<LocalResponseBody, ServiceError> {
        match request {
            LocalRequest::Ping => Ok(LocalResponseBody::Pong),
            LocalRequest::Attach { version, pid, cwd } => {
                if version != LOCAL_PROTOCOL_VERSION {
                    return Err(ServiceError::Invalid(
                        "attach protocol version is unsupported".into(),
                    ));
                }
                if pid == 0 || !cwd.is_absolute() {
                    return Err(ServiceError::Invalid(
                        "attach pid and absolute cwd are required".into(),
                    ));
                }
                let session = {
                    let mut attachments = self.attachments.lock().await;
                    *attachments.entry(pid).or_insert_with(MainSessionId::new)
                };
                Ok(LocalResponseBody::Attached { session })
            }
            LocalRequest::BindClaudeSession {
                main_session,
                claude_session,
            } => {
                if claude_session.trim().is_empty() {
                    return Err(ServiceError::Invalid(
                        "Claude session id must not be empty".into(),
                    ));
                }
                self.claude_sessions
                    .lock()
                    .await
                    .insert(main_session, claude_session);
                Ok(LocalResponseBody::Bound)
            }
            LocalRequest::Nodes => self.node_views().await.map(LocalResponseBody::Nodes),
            LocalRequest::Dispatch(input) => self
                .dispatch(input)
                .await
                .map(LocalResponseBody::Dispatched),
            LocalRequest::Status {
                ids,
                claude_session,
            } => self
                .status(ids, claude_session.as_deref())
                .await
                .map(LocalResponseBody::Status),
            LocalRequest::Inbox {
                since_seq,
                wait_ms,
                kinds,
                all,
                claude_session,
            } => self
                .inbox(
                    since_seq,
                    wait_ms,
                    kinds.as_deref(),
                    if all { None } else { claude_session.as_deref() },
                )
                .await
                .map(LocalResponseBody::Events),
            LocalRequest::Watch { .. } => Err(ServiceError::Unsupported(
                "watch is a streaming request".into(),
            )),
            LocalRequest::Logs {
                subtask,
                tail,
                claude_session,
            } => self
                .logs(subtask, tail, claude_session.as_deref())
                .await
                .map(LocalResponseBody::Logs),
            LocalRequest::Cancel {
                subtask,
                reason,
                claude_session,
            } => {
                self.cancel(subtask, reason, claude_session.as_deref())
                    .await?;
                Ok(LocalResponseBody::Cancelled { subtask })
            }
            LocalRequest::Collect {
                subtask,
                worktree,
                claude_session,
            } => self
                .collect(subtask, worktree, claude_session.as_deref())
                .await
                .map(LocalResponseBody::Collected),
            LocalRequest::Answer {
                subtask,
                question,
                text,
                author,
                claude_session,
            } => self
                .answer(subtask, question, text, author, claude_session.as_deref())
                .await
                .map(LocalResponseBody::Answered),
            LocalRequest::Steer {
                subtask,
                text,
                claude_session,
            } => {
                let delivery = self.steer(subtask, text, claude_session.as_deref()).await?;
                let (delivered, acknowledged) = self
                    .journal(move |journal| journal.runner_delivery_status(delivery.id))
                    .await?;
                Ok(LocalResponseBody::Steered {
                    subtask,
                    delivery_id: delivery.id,
                    delivered,
                    acknowledged,
                })
            }
            LocalRequest::Brief { claude_session } => self
                .brief(&claude_session)
                .await
                .map(LocalResponseBody::Brief),
            LocalRequest::Invite { name, ttl_seconds } => self
                .invite(name, ttl_seconds)
                .await
                .map(LocalResponseBody::Invited),
            LocalRequest::Approve { name } => self.node_lifecycle(name, NodeAction::Approve).await,
            LocalRequest::Pause { name } => self.node_lifecycle(name, NodeAction::Pause).await,
            LocalRequest::Resume { name } => self.node_lifecycle(name, NodeAction::Resume).await,
            LocalRequest::Remove { name } => self.node_lifecycle(name, NodeAction::Remove).await,
        }
    }

    /// Reads one page of committed events for a watch or inbox request.
    pub async fn inbox(
        &self,
        since_seq: u64,
        wait_ms: u64,
        kinds: Option<&[String]>,
        claude_session: Option<&str>,
    ) -> Result<Vec<Event>, ServiceError> {
        let wait_ms = wait_ms.min(50_000);
        let kinds = kinds.unwrap_or_default().to_vec();
        let claude_session = claude_session.map(str::to_owned);
        let visible = self
            .journal(move |journal| journal.visible_subtask_ids(claude_session.as_deref()))
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let kinds = kinds.clone();
            let visible = visible.clone();
            let events = self
                .journal(move |journal| {
                    let kind_refs = kinds.iter().map(String::as_str).collect::<Vec<_>>();
                    journal.events(EventFilter {
                        since_seq,
                        kinds: &kind_refs,
                        scope: EventScope::Watch(&visible),
                        limit: 512,
                    })
                })
                .await?;
            if !events.is_empty() || wait_ms == 0 || tokio::time::Instant::now() >= deadline {
                return Ok(events);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Sends a committed event stream for a watch request. The caller owns socket
    /// lifetime; cancellation of that connection stops this loop.
    pub async fn watch(
        &self,
        scope: WatchScope,
        since_seq: u64,
        mut send: impl FnMut(Event) -> bool,
    ) -> Result<(), ServiceError> {
        let subtask_scope = match scope {
            WatchScope::All => Vec::new(),
            WatchScope::Dispatch(dispatch) => {
                self.journal(move |journal| {
                    Ok(journal
                        .list_subtasks()?
                        .into_iter()
                        .filter(|subtask| subtask.dispatch == dispatch)
                        .map(|subtask| subtask.id)
                        .collect::<Vec<_>>())
                })
                .await?
            }
            WatchScope::Subtask(subtask) => vec![subtask],
        };
        let mut cursor = since_seq;
        loop {
            let ids = subtask_scope.clone();
            let events = self
                .journal(move |journal| {
                    let scope = if ids.is_empty() {
                        EventScope::All
                    } else {
                        EventScope::Watch(&ids)
                    };
                    journal.events(EventFilter {
                        since_seq: cursor,
                        kinds: &[],
                        scope,
                        limit: 256,
                    })
                })
                .await?;
            for event in events {
                cursor = cursor.max(event.seq);
                if !send(event) {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Reconciles the durable node lease, owner-return policy and queued work.
    ///
    /// This pass is intentionally idempotent: node sessions are process-local,
    /// while every state change is committed by the journal before a new driver
    /// is started. A hub restart therefore cannot reassign work until the node
    /// or a clean alternative has had one full heartbeat window to reconnect.
    pub async fn reconcile_nodes(&self) -> Result<(), ServiceError> {
        let now = Timestamp::now();
        let timeout_ms = i64::try_from(self.heartbeat_timeout.as_millis()).unwrap_or(i64::MAX);
        let mut nodes = self.journal(|journal| journal.list_nodes()).await?;
        let mut lost = Vec::new();
        let mut restore = Vec::new();
        let mut owner_busy = Vec::new();
        let mut idle_again = Vec::new();
        let mut returned = Vec::new();

        {
            let mut observations = self.node_observations.lock().await;
            for node in &mut nodes {
                let online = self.nodes.is_online(node.id);
                let observation = observations.entry(node.id).or_insert(NodeObservation {
                    online,
                    presence: node.presence,
                    state_since: now,
                });
                let was_online = observation.online;
                if observation.online != online {
                    observation.state_since = now;
                }
                let previous_presence = observation.presence;
                observation.online = online;
                observation.presence = node.presence;

                if online && !was_online && node.state == NodeState::Lost {
                    restore.push(node.id);
                    node.state = if node.policy.only_when_idle
                        && matches!(node.presence, OwnerPresence::Present)
                    {
                        owner_busy.push(node.id);
                        NodeState::OwnerBusy
                    } else {
                        NodeState::Active
                    };
                }

                if online && node.policy.only_when_idle {
                    if matches!(previous_presence, OwnerPresence::Away { .. })
                        && matches!(node.presence, OwnerPresence::Present)
                        && node.state == NodeState::Active
                    {
                        node.state = NodeState::OwnerBusy;
                        owner_busy.push(node.id);
                        returned.push((node.id, node.policy.on_owner_return));
                    } else if matches!(previous_presence, OwnerPresence::Present)
                        && matches!(node.presence, OwnerPresence::Away { .. })
                        && node.state == NodeState::OwnerBusy
                    {
                        node.state = NodeState::Active;
                        idle_again.push(node.id);
                    }
                }

                if matches!(node.state, NodeState::Active | NodeState::OwnerBusy) {
                    let lease_floor = node.last_seen.0.max(observation.state_since.0);
                    if now.0.saturating_sub(lease_floor) >= timeout_ms {
                        if online {
                            self.nodes.disconnect(node.id);
                        }
                        observation.online = false;
                        observation.state_since = now;
                        node.state = NodeState::Lost;
                        lost.push(node.id);
                    }
                }
            }
        }

        let lost_ids = lost.iter().copied().collect::<HashSet<_>>();
        returned.retain(|(node, _)| !lost_ids.contains(node));

        let lost_for_journal = lost.clone();
        for id in lost_for_journal {
            if let Err(error) = self
                .journal(move |journal| journal.mark_node_lost(id))
                .await
            {
                tracing::debug!(%error, %id, "node was already moved out of a leaseable state");
            }
        }
        for id in restore {
            self.journal(move |journal| journal.restore_lost_node(id))
                .await?;
        }
        for id in owner_busy {
            self.journal(move |journal| journal.mark_node_owner_busy(id))
                .await?;
        }
        for id in idle_again {
            self.journal(move |journal| journal.restore_idle_node(id))
                .await?;
        }
        for id in lost {
            self.reassign_tasks_from_node(id, ReassignReason::NodeLost)
                .await?;
        }
        for (id, policy) in returned {
            if policy != OnReturn::FinishRunning {
                self.reassign_tasks_from_node(id, ReassignReason::OwnerReturned)
                    .await?;
            }
        }

        self.schedule_pending_work().await
    }

    /// Moves unfinished work off a node and cancels local and remote activity
    /// before the scheduler is allowed to place the same subtask elsewhere.
    async fn reassign_tasks_from_node(
        &self,
        node: tagteam_core::NodeId,
        reason: ReassignReason,
    ) -> Result<(), ServiceError> {
        let tasks = self.journal(|journal| journal.list_subtasks()).await?;
        for task in tasks {
            if task.node != Some(node)
                || !matches!(
                    task.state,
                    SubtaskState::Preparing
                        | SubtaskState::Running
                        | SubtaskState::WaitingOnMain { .. }
                        | SubtaskState::Collecting
                )
            {
                continue;
            }
            let id = task.id;
            let transition = self
                .journal(move |journal| {
                    journal.transition(
                        id,
                        SubtaskInput::Reassign { from: node, reason },
                        Timestamp::now(),
                    )
                })
                .await;
            let Ok(_) = transition else {
                continue;
            };
            self.cancel_task_work(id, "subtask is being reassigned")
                .await;
            self.cleanup_workspace(&task, true).await;
        }
        Ok(())
    }

    /// Attempts to place queued/reassigning work and restarts any durable
    /// Preparing or Running task whose in-memory driver disappeared.
    async fn schedule_pending_work(&self) -> Result<(), ServiceError> {
        let _placement = self.placement.lock().await;
        let mut nodes = self.journal(|journal| journal.list_nodes()).await?;
        let tasks = self.journal(|journal| journal.list_subtasks()).await?;
        refresh_node_running(&mut nodes, &tasks);
        let mut account_used = tasks
            .iter()
            .filter(|task| task.state.occupies_worker_slot())
            .count();
        let online_nodes = nodes
            .iter()
            .filter(|node| self.nodes.is_online(node.id))
            .cloned()
            .collect::<Vec<_>>();

        for task in &tasks {
            match task.state {
                SubtaskState::Queued | SubtaskState::Reassigning { .. } => {
                    if account_used >= self.max_concurrent_per_account {
                        continue;
                    }
                    let Placement::Placed(node) =
                        place(&task.spec, &task.repo_id, &online_nodes, &[])
                    else {
                        continue;
                    };
                    account_used = account_used.saturating_add(1);
                    let id = task.id;
                    self.journal(move |journal| {
                        journal
                            .transition(id, SubtaskInput::Placed { node }, Timestamp::now())
                            .map(|_| ())
                    })
                    .await?;
                    self.ensure_driver(id, node).await;
                }
                SubtaskState::Preparing | SubtaskState::Running => {
                    if let Some(node) = task.node
                        && self.nodes.is_online(node)
                    {
                        self.ensure_driver(task.id, node).await;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Starts at most one process-local driver for a durable subtask attempt.
    async fn ensure_driver(&self, id: SubtaskId, node: tagteam_core::NodeId) -> bool {
        if !self.drivers.lock().await.insert(id) {
            return false;
        }
        let service = self.clone();
        tokio::spawn(async move {
            service.supervise_driver(id, node).await;
            service.drivers.lock().await.remove(&id);
        });
        true
    }

    async fn supervise_driver(&self, id: SubtaskId, node: tagteam_core::NodeId) {
        match self.drive_task(id, node).await {
            Ok(()) => {}
            Err(ServiceError::NodePlane(NodePlaneError::Offline | NodePlaneError::Timeout))
            | Err(ServiceError::Invalid(_)) => {
                // A node lease can disappear mid-call. The durable state stays
                // authoritative; reconciliation will resume or reassign it.
            }
            Err(error) => {
                let reason = match &error {
                    ServiceError::Preparation(_) => {
                        FailReason::Bundle(bounded_error(&error.to_string()))
                    }
                    _ => FailReason::Runner(bounded_error(&error.to_string())),
                };
                tracing::warn!(subtask = %id, node = %node, %error, "subtask driver failed");
                self.fail_if_active(id, reason).await;
            }
        }
    }

    async fn drive_task(
        &self,
        id: SubtaskId,
        node: tagteam_core::NodeId,
    ) -> Result<(), ServiceError> {
        let task = self
            .journal(move |journal| journal.load_subtask(id))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("subtask {id}")))?;
        if task.node != Some(node) {
            return Ok(());
        }
        match task.state {
            SubtaskState::Preparing => self.prepare_subtask(&task, node).await?,
            SubtaskState::Running => return self.run_prepared_subtask(id, node).await,
            _ => return Ok(()),
        }
        let prepared = self
            .journal(move |journal| journal.load_subtask(id))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("subtask {id}")))?;
        if prepared.node == Some(node) && prepared.state == SubtaskState::Running {
            self.run_prepared_subtask(id, node).await?;
        }
        Ok(())
    }

    async fn cancel_task_work(&self, id: SubtaskId, reason: &str) {
        let runner = self.runners.lock().await.get(&id).cloned();
        if let Some(runner) = runner {
            let _ = runner
                .send(HubToRunnerMessage::Cancel {
                    reason: reason.to_owned(),
                })
                .await;
        }
        let active = self.active_commands.lock().await.get(&id).cloned();
        if let Some((node, command_id)) = active {
            let _ = self.nodes.cancel_command(node, &command_id, reason).await;
        }
        let _ = self
            .journal(move |journal| journal.cancel_subtask_commands(id).map(|_| ()))
            .await;
    }

    /// Best-effort removal of a terminal workspace and its process-local
    /// sandbox resources. The command is journaled by the node plane, so an
    /// offline node receives it on reconnect; terminal state never waits on
    /// cache retention or a node acknowledgement.
    async fn cleanup_workspace(&self, task: &Subtask, keep_cache: bool) {
        let Some(node) = task.node else {
            return;
        };
        let command = Proto::Command {
            command_id: format!("cleanup-{}-a{}", task.id, task.attempt),
            subtask_id: task.id.to_string(),
            workspace: task.id.to_string(),
            op: Some(Proto::command::Op::Cleanup(Proto::Cleanup { keep_cache })),
        };
        let timeout = Duration::from_secs(5).min(task.spec.limits.command_timeout);
        match self.nodes.execute_command(node, command, timeout).await {
            Ok(Proto::CommandResult {
                result: Some(Proto::command_result::Result::Done(done)),
                ..
            }) if done.ok => {}
            Ok(result) => {
                tracing::debug!(subtask = %task.id, node = %node, ?result, "workspace cleanup was not acknowledged");
            }
            Err(NodePlaneError::Offline | NodePlaneError::Timeout) => {
                tracing::debug!(subtask = %task.id, node = %node, "workspace cleanup deferred until node reconnect");
            }
            Err(error) => {
                tracing::warn!(subtask = %task.id, node = %node, %error, "workspace cleanup failed");
            }
        }
    }

    async fn node_views(&self) -> Result<Vec<NodeView>, ServiceError> {
        let subtasks = self.journal(|journal| journal.list_subtasks()).await?;
        let now = Timestamp::now();
        let all_nodes = self.journal(|journal| journal.list_nodes()).await?;
        Ok(all_nodes
            .into_iter()
            .map(|node| {
                let running_subtasks = subtasks
                    .iter()
                    .filter(|subtask| subtask.node == Some(node.id) && !subtask.state.is_terminal())
                    .map(|subtask| subtask.id)
                    .collect();
                let age_ms = now.0.saturating_sub(node.last_seen.0);
                NodeView {
                    node,
                    last_seen_age: Duration::from_millis(u64::try_from(age_ms).unwrap_or(0)),
                    running_subtasks,
                }
            })
            .collect())
    }

    async fn dispatch(&self, input: DispatchInput) -> Result<DispatchReply, ServiceError> {
        if input.subtasks.is_empty() {
            return Err(ServiceError::Invalid(
                "dispatch needs at least one subtask".into(),
            ));
        }
        if input.subtasks.len() > 8 {
            return Err(ServiceError::Invalid(
                "dispatch accepts at most eight subtasks".into(),
            ));
        }
        if input
            .claude_session
            .as_deref()
            .is_some_and(|session| session.trim().is_empty())
        {
            return Err(ServiceError::Invalid(
                "Claude session id must not be empty".into(),
            ));
        }

        let DispatchInput {
            main_session,
            claude_session,
            repo,
            subtasks: raw_subtasks,
            spread,
        } = input;
        let repository_root = repo.canonicalize().map_err(|error| {
            ServiceError::Invalid(format!("dispatch repository is not accessible: {error}"))
        })?;
        let repository = Repository::open(&repository_root)?;
        if repository.path() != repository_root {
            return Err(ServiceError::Invalid(
                "dispatch repository must be the repository root".into(),
            ));
        }
        let repository_path = repository.path().to_path_buf();

        let mut subtasks = raw_subtasks;
        for spec in &mut subtasks {
            normalize_and_validate_spec(spec, &repository_path)?;
        }

        // Resolve every distinct snapshot before creating durable work. A bad
        // revision or inaccessible worktree must not leave an empty dispatch.
        let mut snapshots: HashMap<(String, bool), ResolvedSnapshot> = HashMap::new();
        for spec in &subtasks {
            let cache_key = (spec.base_ref.clone(), spec.include_uncommitted);
            if let Entry::Vacant(entry) = snapshots.entry(cache_key) {
                let resolved = resolve_snapshot(
                    repository.clone(),
                    spec.base_ref.clone(),
                    spec.include_uncommitted,
                )
                .await?;
                entry.insert(resolved);
            }
        }

        let id = DispatchId::new();
        let created = Timestamp::now();
        let dispatch = Dispatch {
            id,
            main_session,
            claude_session,
            repo: repository_path.clone(),
            created,
        };
        self.journal(move |journal| journal.create_dispatch(&dispatch))
            .await?;

        let _placement = self.placement.lock().await;
        let mut nodes = self.journal(|journal| journal.list_nodes()).await?;
        let existing_tasks = self.journal(|journal| journal.list_subtasks()).await?;
        refresh_node_running(&mut nodes, &existing_tasks);
        let mut account_used = existing_tasks
            .iter()
            .filter(|task| task.state.occupies_worker_slot())
            .count();
        let mut siblings = Vec::new();
        let mut placements = Vec::with_capacity(subtasks.len());
        let mut excluded_paths = HashSet::new();

        for spec in subtasks {
            let cache_key = (spec.base_ref.clone(), spec.include_uncommitted);
            let snapshot = snapshots
                .get(&cache_key)
                .cloned()
                .ok_or_else(|| ServiceError::Internal)?;
            excluded_paths.extend(snapshot.excluded_paths.iter().cloned());

            let subtask_id = SubtaskId::new();
            let branch = branch_name(&spec.title, subtask_id);
            let task = Subtask {
                id: subtask_id,
                dispatch: id,
                spec: spec.clone(),
                state: SubtaskState::Placing,
                node: None,
                attempt: 0,
                base: snapshot.base,
                repo_id: snapshot.repo_id.clone(),
                branch,
                runner_session: None,
                created,
                started: None,
                finished: None,
                usage: tagteam_core::Usage::default(),
            };
            self.journal(move |journal| journal.create_subtask(&task))
                .await?;

            let sibling_scope = if spread { siblings.as_slice() } else { &[] };
            let placement = if account_used >= self.max_concurrent_per_account {
                Placement::Queued(vec![NoFitReason::AccountAtCapacity])
            } else {
                place(&spec, &snapshot.repo_id, &nodes, sibling_scope)
            };
            let transition = match &placement {
                Placement::Placed(node) => {
                    account_used = account_used.saturating_add(1);
                    siblings.push(*node);
                    if let Some(selected) = nodes.iter_mut().find(|node_view| node_view.id == *node)
                    {
                        selected.capacity.running = selected.capacity.running.saturating_add(1);
                    }
                    SubtaskInput::Placed { node: *node }
                }
                Placement::Queued(reasons) => SubtaskInput::NoFit {
                    reasons: reasons.clone(),
                },
            };
            self.journal(move |journal| {
                journal.transition(subtask_id, transition, Timestamp::now())
            })
            .await?;
            if let Placement::Placed(node) = placement {
                self.ensure_driver(subtask_id, node).await;
            }
            placements.push((subtask_id, placement));
        }

        let mut excluded_paths = excluded_paths.into_iter().collect::<Vec<_>>();
        excluded_paths.sort();
        Ok(DispatchReply {
            dispatch: id,
            subtasks: placements,
            excluded_paths,
            watch_command: format!("tagteam watch --dispatch {id}"),
        })
    }

    /// Creates a verified source bundle, uploads it to the hub blob store and
    /// asks the assigned node to prepare its private workspace. The command ID
    /// is deterministic so node reconnects cannot replay a partial checkout.
    async fn prepare_subtask(
        &self,
        task: &Subtask,
        node: tagteam_core::NodeId,
    ) -> Result<(), ServiceError> {
        if task.state != SubtaskState::Preparing || task.node != Some(node) {
            return Ok(());
        }
        let blob = self.create_source_bundle(task).await?;
        let command_id = format!("prepare-{}-a{}", task.id, task.attempt);
        let command = Proto::Command {
            command_id,
            subtask_id: task.id.to_string(),
            workspace: task.id.to_string(),
            op: Some(Proto::command::Op::Prepare(Proto::PrepareWorkspace {
                bundle: Some(Proto::BlobRef {
                    sha256: blob.sha256.to_vec(),
                    size: blob.size,
                }),
                repo_id: task.repo_id.0.0.clone(),
                base: task.base.0.clone(),
                branch: task.branch.clone(),
                profile: Some(Proto::SandboxProfile {
                    writable: Vec::new(),
                    readable: Vec::new(),
                    egress: task.spec.egress.clone(),
                    cpu_cores: f32::from(task.spec.needs.cores),
                    memory: task.spec.needs.memory.0,
                    pids: 64,
                    disk: task.spec.needs.disk.0,
                }),
            })),
        };
        let timeout = std::cmp::max(task.spec.limits.command_timeout, Duration::from_secs(60));
        let id = task.id;
        let retry_until = std::time::Instant::now() + task.spec.limits.wall_clock;
        let mut retry_delay = Duration::from_millis(250);
        loop {
            let current = self
                .journal(move |journal| journal.load_subtask_state(id))
                .await?;
            if current != Some(SubtaskState::Preparing) {
                return Ok(());
            }
            match self
                .nodes
                .execute_command(node, command.clone(), timeout)
                .await
            {
                Ok(result) => {
                    ensure_prepare_succeeded(&result)?;
                    self.journal(move |journal| {
                        journal
                            .transition(id, SubtaskInput::Prepared, Timestamp::now())
                            .map(|_| ())
                    })
                    .await?;
                    return Ok(());
                }
                Err(NodePlaneError::Offline | NodePlaneError::Timeout)
                    if std::time::Instant::now() < retry_until =>
                {
                    // The command is durably journaled before it leaves the hub.
                    // Retrying the same ID recovers either a result stored while
                    // the node was reconnecting or a redelivery of the same work.
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(5));
                }
                Err(NodePlaneError::Offline | NodePlaneError::Timeout) => {
                    return Err(ServiceError::Preparation(format!(
                        "node {node} did not acknowledge workspace preparation within the wall-clock limit"
                    )));
                }
                Err(error) => return Err(ServiceError::NodePlane(error)),
            }
        }
    }

    async fn create_source_bundle(&self, task: &Subtask) -> Result<BlobRef, ServiceError> {
        let repository = task.spec.repo.clone();
        let base = task.base.0.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            let repository = Repository::open(&repository)?;
            let directory = tempfile::tempdir().map_err(|error| tagteam_git::GitError::Io {
                path: std::env::temp_dir(),
                source: error,
            })?;
            let bundle = repository.create_bundle(
                &base,
                directory.path().join("workspace.bundle"),
                &[],
                None,
            )?;
            repository.verify_bundle(&bundle.path)?;
            if bundle.bytes > MAX_BLOB_BYTES {
                return Err(ServiceError::Invalid(format!(
                    "source bundle is {} bytes; the node limit is {MAX_BLOB_BYTES}",
                    bundle.bytes
                )));
            }
            Ok::<Vec<u8>, ServiceError>(fs::read(&bundle.path).map_err(|source| {
                tagteam_git::GitError::Io {
                    path: bundle.path.clone(),
                    source,
                }
            })?)
        })
        .await
        .map_err(|_| ServiceError::Worker)??;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > MAX_BLOB_BYTES {
            return Err(ServiceError::Invalid(format!(
                "source bundle exceeds the {MAX_BLOB_BYTES}-byte node limit"
            )));
        }
        let root = self.paths.blobs.clone();
        tokio::task::spawn_blocking(move || {
            BlobStore::open(root)
                .and_then(|store| store.put(&bytes))
                .map_err(ServiceError::from)
        })
        .await
        .map_err(|_| ServiceError::Worker)?
    }

    /// Starts the Agent SDK runner for a prepared subtask and bridges its remote
    /// tools to the assigned node. The bridge owns the runner child; a dropped
    /// bridge, cancellation, or wall-clock expiry kills it rather than leaving an
    /// orphaned model loop behind.
    async fn run_prepared_subtask(
        &self,
        id: SubtaskId,
        node: tagteam_core::NodeId,
    ) -> Result<(), ServiceError> {
        let task = self
            .journal(move |journal| journal.load_subtask(id))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("subtask {id}")))?;
        if task.state != SubtaskState::Running || task.node != Some(node) {
            return Ok(());
        }
        let mut process = RunnerProcess::start(RunnerStart {
            program: self.runner_program.clone(),
            args: self.runner_args.clone(),
            subtask: id,
            prompt: task.spec.prompt.clone(),
            system_append: runner_system_append(&task),
            cwd: task.spec.repo.clone(),
            max_turns: task.spec.limits.max_turns,
            wall_clock_ms: wall_clock_ms(&task.spec.limits),
            tool_timeout_ms: mcp_timeout_ms(&task.spec.limits),
            resume: task.runner_session.clone(),
        })
        .await
        .map_err(|error| ServiceError::Runner(error.to_string()))?;

        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        self.runners.lock().await.insert(id, sender);
        let deadline = Duration::from_millis(wall_clock_ms(&task.spec.limits))
            .saturating_add(Duration::from_secs(60));
        let bridged = tokio::time::timeout(
            deadline,
            self.bridge_runner(&task, node, &mut process, &mut receiver),
        )
        .await;
        self.runners.lock().await.remove(&id);
        match bridged {
            Ok(result) => {
                process.kill().await;
                result
            }
            Err(_) => {
                process.kill().await;
                Err(ServiceError::Runner(
                    "runner exceeded its wall-clock budget".into(),
                ))
            }
        }
    }

    async fn bridge_runner(
        &self,
        task: &Subtask,
        node: tagteam_core::NodeId,
        process: &mut RunnerProcess,
        receiver: &mut tokio::sync::mpsc::Receiver<HubToRunnerMessage>,
    ) -> Result<(), ServiceError> {
        loop {
            tokio::select! {
                message = process.next() => {
                    let message = message.map_err(|error| ServiceError::Runner(error.to_string()))?;
                    match message {
                        RunnerToHubMessage::Ready {
                            session_id,
                            claude_code_version,
                            tools,
                            mcp_servers,
                        } => {
                            if mcp_servers.iter().map(String::as_str).collect::<Vec<_>>() != ["tagteam"]
                                || tools.iter().any(|tool| !allowed_runner_tool(tool))
                            {
                                return Err(ServiceError::Runner(
                                    "runner reported an unexpected tool or MCP server".into(),
                                ));
                            }
                            let id = task.id;
                            let state = self
                                .journal(move |journal| journal.load_subtask_state(id))
                                .await?;
                            if !matches!(state, Some(SubtaskState::Running)) {
                                return Ok(());
                            }
                            let pending = self
                                .journal(move |journal| journal.pending_runner_deliveries(id))
                                .await?;
                            for delivery in pending {
                                process
                                    .send(delivery.message)
                                    .await
                                    .map_err(|error| ServiceError::Runner(error.to_string()))?;
                                let delivery_id = delivery.id;
                                self.journal(move |journal| {
                                    journal.mark_runner_delivery_sent(delivery_id, Timestamp::now())
                                })
                                .await?;
                            }
                            self.journal(move |journal| {
                                journal
                                    .transition(
                                        id,
                                        SubtaskInput::RunnerStarted {
                                            session: session_id.clone(),
                                        },
                                        Timestamp::now(),
                                    )
                                    .map(|_| ())
                            })
                            .await?;
                            self.record_progress(
                                task.id,
                                format!("runner ready ({claude_code_version})"),
                                None,
                            )
                            .await?;
                        }
                        RunnerToHubMessage::ToolCall { call_id, call } => {
                            if let RemoteCall::Finish {
                                outcome,
                                summary,
                                assumptions,
                                checks: _,
                            } = call
                            {
                                let result = self
                                    .finalize_finish(
                                        task,
                                        node,
                                        FinishRequest {
                                            outcome,
                                            summary,
                                            assumptions,
                                        },
                                    )
                                    .await?;
                                let _ = process
                                    .send(HubToRunnerMessage::ToolResult {
                                        call_id,
                                        text: format!("finish accepted: {}", result.summary),
                                        is_error: false,
                                    })
                                    .await;
                                return Ok(());
                            }

                            if let RemoteCall::AskMain {
                                question_id,
                                text,
                                options,
                                context,
                            } = call
                            {
                                let question = Question {
                                    id: question_id,
                                    subtask: task.id,
                                    text: bounded_text(text.trim(), 8_000),
                                    options: options
                                        .into_iter()
                                        .take(16)
                                        .map(|option| bounded_text(option.trim(), 512))
                                        .filter(|option| !option.is_empty())
                                        .collect(),
                                    context: context.map(|context| bounded_text(&context, 4_000)),
                                    deadline: Timestamp::now()
                                        .checked_add(task.spec.limits.question_deadline),
                                };
                                if question.text.is_empty() {
                                    process
                                        .send(HubToRunnerMessage::ToolResult {
                                            call_id,
                                            text: "question text must not be empty".into(),
                                            is_error: true,
                                        })
                                        .await
                                        .map_err(|error| {
                                            ServiceError::Runner(error.to_string())
                                        })?;
                                    continue;
                                }
                                let id = task.id;
                                let call_id_for_store = call_id.clone();
                                self.journal(move |journal| {
                                    journal
                                        .transition(
                                            id,
                                            SubtaskInput::Ask {
                                                question,
                                                call_id: call_id_for_store,
                                            },
                                            Timestamp::now(),
                                        )
                                        .map(|_| ())
                                })
                                .await?;
                                // The runner tool promise remains pending until an
                                // answer (or timeout guidance) arrives on the
                                // receiver branch below.
                                continue;
                            }

                            let started = std::time::Instant::now();
                            let result: Result<RemoteOutcome, ServiceError> = self
                                .execute_remote_call(task, node, &call_id, &call)
                                .await;
                            let (text, is_error) = match result {
                                Ok(outcome) => (outcome.text, outcome.is_error),
                                Err(error) => (bounded_error(&error.to_string()), true),
                            };
                            let tool = remote_tool_name(&call);
                            let id = task.id;
                            self.journal(move |journal| {
                                journal
                                    .record_event(
                                        id,
                                        EventKind::ToolRan {
                                            tool: tool.into(),
                                            summary: if is_error {
                                                "tool returned an error".into()
                                            } else {
                                                "tool completed".into()
                                            },
                                            ok: !is_error,
                                            took: started.elapsed(),
                                        },
                                        Timestamp::now(),
                                    )
                                    .map(|_| ())
                            })
                            .await?;
                            process
                                .send(HubToRunnerMessage::ToolResult {
                                    call_id,
                                    text,
                                    is_error,
                                })
                                .await
                                .map_err(|error| ServiceError::Runner(error.to_string()))?;
                        }
                        RunnerToHubMessage::Usage { usage } => {
                            let id = task.id;
                            self.journal(move |journal| journal.set_usage(id, usage))
                                .await?;
                        }
                        RunnerToHubMessage::Note { text } => {
                            self.record_progress(task.id, text, None).await?;
                        }
                        RunnerToHubMessage::RateLimited { detail } => {
                            self.record_progress(
                                task.id,
                                format!("model rate limited: {detail}"),
                                None,
                            )
                            .await?;
                        }
                        RunnerToHubMessage::Exited {
                            subtype,
                            error,
                            session_id: _,
                            cost_usd: _,
                        } => {
                            return Err(ServiceError::Runner(format!(
                                "runner exited before finish ({subtype}{})",
                                error.map(|error| format!(": {error}")).unwrap_or_default()
                            )));
                        }
                        RunnerToHubMessage::DeliveryAck { delivery_id } => {
                            self.journal(move |journal| {
                                journal.acknowledge_runner_delivery(delivery_id, Timestamp::now())
                            })
                            .await?;
                        }
                    }
                }
                inbound = receiver.recv() => {
                    match inbound {
                        Some(message) => {
                            let cancelling = matches!(message, HubToRunnerMessage::Cancel { .. });
                            let delivery_id = match &message {
                                HubToRunnerMessage::Answer { delivery_id, .. }
                                | HubToRunnerMessage::Steer { delivery_id, .. } => Some(*delivery_id),
                                _ => None,
                            };
                            process
                                .send(message)
                                .await
                                .map_err(|error| ServiceError::Runner(error.to_string()))?;
                            if let Some(delivery_id) = delivery_id {
                                self.journal(move |journal| {
                                    journal.mark_runner_delivery_sent(delivery_id, Timestamp::now())
                                })
                                .await?;
                            }
                            if cancelling {
                                return Ok(());
                            }
                        }
                        None => return Ok(()),
                    }
                }
            }
        }
    }

    async fn execute_remote_call(
        &self,
        task: &Subtask,
        node: tagteam_core::NodeId,
        call_id: &str,
        call: &RemoteCall,
    ) -> Result<RemoteOutcome, ServiceError> {
        let id = task.id;
        let state = self
            .journal(move |journal| journal.load_subtask_state(id))
            .await?;
        if !matches!(state, Some(SubtaskState::Running)) {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }
        if let RemoteCall::ReportProgress { note, percent } = call {
            self.record_progress(task.id, note.clone(), *percent)
                .await?;
            return Ok(RemoteOutcome {
                text: "progress recorded".into(),
                is_error: false,
            });
        }
        let command = remote_command(task, call_id, call)?;
        let result = self.execute_tracked_command(task, node, command).await?;
        render_remote_result(&result, task)
    }

    /// Executes a node command only while the subtask is still running, and
    /// records it as cancellable for the duration of the call. Cancellation can
    /// race with finish/collection, so the state is checked again after the
    /// command is registered and before its result is accepted.
    async fn execute_tracked_command(
        &self,
        task: &Subtask,
        node: tagteam_core::NodeId,
        command: Proto::Command,
    ) -> Result<Proto::CommandResult, ServiceError> {
        if !self.is_running(task.id).await? {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }
        let command_id = command.command_id.clone();
        self.active_commands
            .lock()
            .await
            .insert(task.id, (node, command_id.clone()));
        if !self.is_running(task.id).await? {
            self.active_commands.lock().await.remove(&task.id);
            let _ = self
                .nodes
                .cancel_command(node, &command_id, "subtask cancelled")
                .await;
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }
        let result = self
            .nodes
            .execute_command(node, command, command_wait(task))
            .await;
        self.active_commands.lock().await.remove(&task.id);
        result.map_err(ServiceError::from)
    }

    async fn is_running(&self, id: SubtaskId) -> Result<bool, ServiceError> {
        Ok(matches!(
            self.journal(move |journal| journal.load_subtask_state(id))
                .await?,
            Some(SubtaskState::Running)
        ))
    }

    async fn finalize_finish(
        &self,
        task: &Subtask,
        node: tagteam_core::NodeId,
        finish: FinishRequest,
    ) -> Result<ResultSummary, ServiceError> {
        let id = task.id;
        if !self.is_running(id).await? {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }

        let command_id = format!("collect-{}-a{}", task.id, task.attempt);
        let collect = Proto::Command {
            command_id,
            subtask_id: task.id.to_string(),
            workspace: task.id.to_string(),
            op: Some(Proto::command::Op::Collect(Proto::Collect {
                base: task.base.0.clone(),
                message: format!("tagteam: {}", bounded_text(&task.spec.title, 240)),
            })),
        };
        let result = self.execute_tracked_command(task, node, collect).await?;
        let blob = match result.result {
            Some(Proto::command_result::Result::Collected(blob)) => {
                let digest: [u8; 32] = blob
                    .sha256
                    .as_slice()
                    .try_into()
                    .map_err(|_| ServiceError::Invalid("result digest is invalid".into()))?;
                BlobRef {
                    sha256: digest,
                    size: blob.size,
                }
            }
            Some(Proto::command_result::Result::Error(error)) => {
                return Err(ServiceError::Runner(bounded_error(&error.message)));
            }
            _ => {
                return Err(ServiceError::Runner(
                    "node returned an unexpected result for collection".into(),
                ));
            }
        };
        if blob.size > MAX_BLOB_BYTES {
            return Err(ServiceError::Invalid(
                "result bundle exceeds the shared blob limit".into(),
            ));
        }
        if !self.is_running(id).await? {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }

        let root = self.paths.blobs.clone();
        let read_blob = blob.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            BlobStore::open(root)
                .and_then(|store| store.read(&read_blob))
                .map_err(ServiceError::from)
        })
        .await
        .map_err(|_| ServiceError::Worker)??;
        let bundle = tempfile::NamedTempFile::new()
            .map_err(|error| ServiceError::Runner(error.to_string()))?;
        fs::write(bundle.path(), &bytes)
            .map_err(|error| ServiceError::Runner(error.to_string()))?;
        if !self.is_running(id).await? {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }

        let repository_path = task.spec.repo.clone();
        let base = task.base.0.clone();
        let branch = task.branch.clone();
        let destination = format!("result-{}", task.id);
        let bundle_path = bundle.path().to_path_buf();
        let max_patch = task.spec.limits.max_patch.0;
        let (fetched, diffstat, patch_within_limit) = tokio::task::spawn_blocking(move || {
            let repository = Repository::open(repository_path)?;
            repository.verify_bundle(&bundle_path)?;
            let fetched = repository.fetch_bundle(&bundle_path, &branch, &destination)?;
            repository.check_ancestry(&base, &fetched.0)?;
            let diffstat = repository.diffstat(&base, &fetched.0)?;
            let patch_within_limit = repository
                .diff_size_up_to(&base, &fetched.0, max_patch)?
                .is_some();
            Ok::<_, ServiceError>((fetched, diffstat, patch_within_limit))
        })
        .await
        .map_err(|_| ServiceError::Worker)??;
        if !self.is_running(id).await? {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }
        if !patch_within_limit {
            let limit = task.spec.limits.max_patch;
            let estimate = tagteam_core::Bytes(limit.0.saturating_add(1));
            self.journal(move |journal| {
                journal
                    .transition(
                        id,
                        SubtaskInput::Fail {
                            reason: FailReason::PatchTooLarge {
                                limit,
                                actual: estimate,
                            },
                        },
                        Timestamp::now(),
                    )
                    .map(|_| ())
            })
            .await?;
            self.cleanup_workspace(task, true).await;
            return Err(ServiceError::Invalid(format!(
                "result patch exceeds the {limit} limit"
            )));
        }
        let checks = self.recheck_done_when(task, node).await?;
        if !self.is_running(id).await? {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }
        let checks_passed = checks.iter().all(|check| check.passed);
        let outcome =
            if !checks_passed && matches!(finish.outcome, Outcome::Completed | Outcome::NoChange) {
                Outcome::Partial
            } else {
                finish.outcome
            };
        let result = ResultSummary {
            outcome,
            summary: bounded_text(&finish.summary, 4096),
            bundle: Some(blob.clone()),
            branch: Some(task.branch.clone()),
            diffstat,
            assumptions: finish
                .assumptions
                .into_iter()
                .map(|assumption| Assumption {
                    what: bounded_text(&assumption.what, 512),
                    why: bounded_text(&assumption.why, 1024),
                    from_timeout: assumption.from_timeout,
                })
                .collect(),
            checks,
        };

        // Cancellation and landing share this gate. Whichever operation gets
        // here first wins: cancellation leaves no branch, or landing finishes
        // before the cancel request can turn a live task terminal.
        let landing = self.landing.lock().await;
        if !self.is_running(id).await? {
            return Err(ServiceError::Invalid("subtask is no longer running".into()));
        }
        let branch = task.branch.clone();
        let fetched_commit = fetched.0.clone();
        let landing_repository = task.spec.repo.clone();
        tokio::task::spawn_blocking(move || {
            let repository = Repository::open(landing_repository)?;
            repository.create_review_branch(&branch, &fetched_commit)?;
            Ok::<_, ServiceError>(())
        })
        .await
        .map_err(|_| ServiceError::Worker)??;
        // The result branch is durable on the hub now; the borrowed machine no
        // longer needs the workspace. Recorded terminal state waits until this
        // best-effort cleanup command has had its bounded chance to complete.
        self.cleanup_workspace(task, true).await;
        let finished_result = result.clone();
        self.journal(move |journal| {
            journal
                .transition(
                    id,
                    SubtaskInput::Finished {
                        result: finished_result,
                    },
                    Timestamp::now(),
                )
                .map(|_| ())
        })
        .await?;
        self.journal(move |journal| {
            journal
                .transition(
                    id,
                    SubtaskInput::Collected { blob: blob.clone() },
                    Timestamp::now(),
                )
                .map(|_| ())
        })
        .await?;
        let landed_result = result.clone();
        self.journal(move |journal| {
            journal
                .transition(
                    id,
                    SubtaskInput::Landed {
                        result: landed_result,
                    },
                    Timestamp::now(),
                )
                .map(|_| ())
        })
        .await?;
        drop(landing);
        Ok(result)
    }

    async fn recheck_done_when(
        &self,
        task: &Subtask,
        node: tagteam_core::NodeId,
    ) -> Result<Vec<Check>, ServiceError> {
        let mut checks = Vec::with_capacity(task.spec.done_when.len());
        for (index, command) in task.spec.done_when.iter().enumerate() {
            if !self.is_running(task.id).await? {
                return Err(ServiceError::Invalid("subtask is no longer running".into()));
            }
            let command_id = format!("check-{}-a{}-{}", task.id, task.attempt, index);
            let result = self
                .execute_tracked_command(
                    task,
                    node,
                    Proto::Command {
                        command_id,
                        subtask_id: task.id.to_string(),
                        workspace: task.id.to_string(),
                        op: Some(Proto::command::Op::Exec(Proto::Exec {
                            argv: vec!["bash".into(), "-lc".into(), command.clone()],
                            cwd: String::new(),
                            env: std::collections::HashMap::new(),
                            stdin: Vec::new(),
                            timeout_ms: command_timeout_ms(&task.spec.limits),
                            output_cap: output_cap(&task.spec.limits),
                        })),
                    },
                )
                .await;
            if !self.is_running(task.id).await? {
                return Err(ServiceError::Invalid("subtask is no longer running".into()));
            }
            let check = match result {
                Ok(Proto::CommandResult {
                    result: Some(Proto::command_result::Result::Exec(exec)),
                    ..
                }) => Check {
                    what: command.clone(),
                    command: Some(command.clone()),
                    passed: exec.exit_code == 0
                        && exec.signal == 0
                        && !exec.timed_out
                        && !exec.killed,
                    output_tail: bounded_text(&render_exec_tail(&exec), 8192),
                },
                Ok(Proto::CommandResult {
                    result: Some(Proto::command_result::Result::Error(error)),
                    ..
                }) => Check {
                    what: command.clone(),
                    command: Some(command.clone()),
                    passed: false,
                    output_tail: bounded_error(&error.message),
                },
                Ok(_) => Check {
                    what: command.clone(),
                    command: Some(command.clone()),
                    passed: false,
                    output_tail: "node returned an unexpected check result".into(),
                },
                Err(error) => Check {
                    what: command.clone(),
                    command: Some(command.clone()),
                    passed: false,
                    output_tail: bounded_error(&error.to_string()),
                },
            };
            checks.push(check);
        }
        Ok(checks)
    }

    async fn record_progress(
        &self,
        id: SubtaskId,
        note: String,
        percent: Option<u8>,
    ) -> Result<(), ServiceError> {
        self.journal(move |journal| {
            journal
                .record_event(
                    id,
                    EventKind::Progress {
                        note: bounded_text(&note, 1024),
                        percent,
                    },
                    Timestamp::now(),
                )
                .map(|_| ())
        })
        .await
    }

    async fn fail_if_active(&self, id: SubtaskId, reason: FailReason) {
        let task = self.journal(move |journal| journal.load_subtask(id)).await;
        let Ok(Some(task)) = task else {
            return;
        };
        if task.state.is_terminal() {
            return;
        }
        let transition = self
            .journal(move |journal| {
                journal
                    .transition(id, SubtaskInput::Fail { reason }, Timestamp::now())
                    .map(|_| ())
            })
            .await;
        if transition.is_ok() {
            self.cleanup_workspace(&task, true).await;
        }
    }

    async fn status(
        &self,
        ids: Option<Vec<SubtaskId>>,
        claude_session: Option<&str>,
    ) -> Result<StatusReply, ServiceError> {
        let wanted = ids.map(|ids| ids.into_iter().collect::<std::collections::HashSet<_>>());
        let subtasks = match claude_session {
            Some(session) => {
                let session = session.to_owned();
                self.journal(move |journal| journal.list_subtasks_for_session(&session))
                    .await?
            }
            None => self.journal(|journal| journal.list_subtasks()).await?,
        };
        let events = self
            .journal(|journal| journal.events(EventFilter::default()))
            .await?;
        let mut statuses = Vec::new();
        for subtask in subtasks.into_iter().filter(|subtask| {
            wanted
                .as_ref()
                .is_none_or(|wanted| wanted.contains(&subtask.id))
        }) {
            let question = self
                .journal(move |journal| journal.open_question(subtask.id))
                .await?;
            statuses.push(status_for(&subtask, &events, question.as_ref()));
        }
        Ok(StatusReply { subtasks: statuses })
    }

    async fn logs(
        &self,
        subtask: SubtaskId,
        tail: u32,
        claude_session: Option<&str>,
    ) -> Result<LogsReply, ServiceError> {
        self.owned_subtask(subtask, claude_session).await?;
        let events = self
            .journal(move |journal| {
                journal.events(EventFilter {
                    since_seq: 0,
                    kinds: &[],
                    scope: EventScope::Subtask(subtask),
                    limit: 4_096,
                })
            })
            .await?;
        let mut lines = events.iter().map(render_event).collect::<Vec<_>>();
        if tail > 0 {
            let tail = usize::try_from(tail).unwrap_or(usize::MAX);
            if lines.len() > tail {
                lines = lines.split_off(lines.len() - tail);
            }
        }
        Ok(LogsReply { subtask, lines })
    }

    async fn cancel(
        &self,
        subtask: SubtaskId,
        reason: String,
        claude_session: Option<&str>,
    ) -> Result<(), ServiceError> {
        if reason.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "cancel reason must not be empty".into(),
            ));
        }
        self.owned_subtask(subtask, claude_session).await?;
        let landing = self.landing.lock().await;
        let current = self
            .journal(move |journal| journal.load_subtask_state(subtask))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("subtask {subtask}")))?;
        if current == SubtaskState::Cancelled {
            return Ok(());
        }
        if current.is_terminal() {
            return Err(ServiceError::Invalid(format!(
                "subtask {subtask} is already terminal"
            )));
        }
        self.journal(move |journal| {
            journal
                .transition(
                    subtask,
                    tagteam_core::state::SubtaskInput::Cancel { reason },
                    Timestamp::now(),
                )
                .map(|_| ())
        })
        .await?;
        drop(landing);
        let runner = self.runners.lock().await.get(&subtask).cloned();
        if let Some(runner) = runner {
            let _ = runner
                .send(HubToRunnerMessage::Cancel {
                    reason: "subtask cancelled".into(),
                })
                .await;
        }
        let active = self.active_commands.lock().await.get(&subtask).cloned();
        if let Some((node, command_id)) = active {
            let _ = self
                .nodes
                .cancel_command(node, &command_id, "subtask cancelled")
                .await;
        }
        if let Ok(Some(task)) = self
            .journal(move |journal| journal.load_subtask(subtask))
            .await
        {
            self.cleanup_workspace(&task, true).await;
        }
        Ok(())
    }

    async fn collect(
        &self,
        subtask: SubtaskId,
        worktree: bool,
        claude_session: Option<&str>,
    ) -> Result<CollectReply, ServiceError> {
        let task = self.owned_subtask(subtask, claude_session).await?;
        let result = self
            .journal(move |journal| journal.load_result_summary(subtask))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("result summary for {subtask}")))?;
        let worktree = if worktree {
            let branch = result
                .branch
                .as_deref()
                .unwrap_or(task.branch.as_str())
                .to_owned();
            let repository = Repository::open(&task.spec.repo)?;
            Some(repository.add_review_worktree(&subtask.to_string(), &branch)?)
        } else {
            None
        };
        Ok(CollectReply {
            subtask,
            result,
            worktree,
        })
    }

    async fn owned_subtask(
        &self,
        subtask: SubtaskId,
        claude_session: Option<&str>,
    ) -> Result<Subtask, ServiceError> {
        let task = self
            .journal(move |journal| journal.load_subtask(subtask))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("subtask {subtask}")))?;
        if let Some(session) = claude_session {
            let session = session.to_owned();
            let owned = self
                .journal(move |journal| journal.subtask_belongs_to_session(subtask, &session))
                .await?;
            if !owned {
                return Err(ServiceError::NotFound(format!("subtask {subtask}")));
            }
        }
        Ok(task)
    }

    async fn answer(
        &self,
        subtask: SubtaskId,
        question: Option<QuestionId>,
        text: String,
        author: AnswerAuthor,
        claude_session: Option<&str>,
    ) -> Result<AnswerReply, ServiceError> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Err(ServiceError::Invalid(
                "answer text must not be empty".into(),
            ));
        }
        if text.len() > 16 * 1024 {
            return Err(ServiceError::Invalid(
                "answer text exceeds the 16 KiB limit".into(),
            ));
        }
        let task = self.owned_subtask(subtask, claude_session).await?;
        let open_id = match task.state {
            SubtaskState::WaitingOnMain { question } => question,
            _ => {
                return Err(ServiceError::Invalid(
                    "subtask is not waiting for an answer".into(),
                ));
            }
        };
        if question.is_some_and(|question| question != open_id) {
            return Err(ServiceError::Invalid(
                "question does not match the open question".into(),
            ));
        }
        let stored = self
            .journal(move |journal| journal.load_question(open_id))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("question {open_id}")))?;
        if stored.answer.is_some() || stored.closed_at.is_some() {
            return Err(ServiceError::Invalid("question is already closed".into()));
        }
        if stored.question.deadline < Timestamp::now() {
            return Err(ServiceError::Invalid(
                "question deadline has passed; the runner will receive the timeout policy".into(),
            ));
        }
        let by = author.answered_by();
        let answer_text = text.clone();
        let answer_by = by.clone();
        let question_text = stored.question.text.clone();
        let call_id = stored.call_id.clone();
        let delivery = self
            .journal(move |journal| {
                journal.answer_with_delivery(
                    subtask,
                    AnswerDeliveryIntent {
                        question: open_id,
                        question_text,
                        call_id,
                        text: answer_text,
                        by: answer_by,
                        timed_out: false,
                    },
                    Timestamp::now(),
                )
            })
            .await?;
        let runner = self.runners.lock().await.get(&subtask).cloned();
        if let Some(runner) = runner {
            let _ = runner.send(delivery.message.clone()).await;
        }
        let delivery_id = delivery.id;
        let (delivered, acknowledged) = self
            .journal(move |journal| journal.runner_delivery_status(delivery_id))
            .await?;
        Ok(AnswerReply {
            delivery_id,
            subtask,
            question: open_id,
            text,
            by,
            delivered,
            acknowledged,
        })
    }

    async fn steer(
        &self,
        subtask: SubtaskId,
        text: String,
        claude_session: Option<&str>,
    ) -> Result<RunnerDelivery, ServiceError> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Err(ServiceError::Invalid(
                "steering text must not be empty".into(),
            ));
        }
        if text.len() > 16 * 1024 {
            return Err(ServiceError::Invalid(
                "steering text exceeds the 16 KiB limit".into(),
            ));
        }
        let task = self.owned_subtask(subtask, claude_session).await?;
        if task.state.is_terminal() {
            return Err(ServiceError::Invalid(
                "cannot steer a terminal subtask".into(),
            ));
        }
        let delivery = self
            .journal(move |journal| journal.steer_with_delivery(subtask, text, Timestamp::now()))
            .await?;
        if let Some(runner) = self.runners.lock().await.get(&subtask).cloned() {
            let _ = runner.send(delivery.message.clone()).await;
        }
        Ok(delivery)
    }

    /// Expires unanswered questions and wakes their runners with the documented
    /// best-judgment policy. Returns the number of questions answered by timeout.
    pub async fn expire_due_questions(&self) -> Result<usize, ServiceError> {
        let now = Timestamp::now();
        let due = self
            .journal(move |journal| journal.due_questions(now))
            .await?;
        let mut expired = 0;
        for stored in due {
            let task = self
                .journal(move |journal| journal.load_subtask(stored.question.subtask))
                .await?;
            let Some(task) = task else {
                continue;
            };
            if task.state
                != (SubtaskState::WaitingOnMain {
                    question: stored.question.id,
                })
            {
                continue;
            }
            let guidance = "No main-session answer arrived before the deadline. Continue using your best judgment, make the smallest safe assumption, and record it in the Finish assumptions with from_timeout=true.";
            let question = stored.question.id;
            let subtask = stored.question.subtask;
            let question_text = stored.question.text.clone();
            let call_id = stored.call_id.clone();
            let result = self
                .journal(move |journal| {
                    journal.answer_with_delivery(
                        subtask,
                        AnswerDeliveryIntent {
                            question,
                            question_text,
                            call_id,
                            text: guidance.into(),
                            by: AnsweredBy::Timeout {
                                guess: guidance.into(),
                            },
                            timed_out: true,
                        },
                        Timestamp::now(),
                    )
                })
                .await;
            let Ok(delivery) = result else { continue };
            expired += 1;
            let runner = self
                .runners
                .lock()
                .await
                .get(&stored.question.subtask)
                .cloned();
            if let Some(runner) = runner {
                let _ = runner.send(delivery.message).await;
            }
        }
        Ok(expired)
    }

    async fn brief(&self, claude_session: &str) -> Result<BriefReply, ServiceError> {
        if claude_session.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "Claude session id must not be empty".into(),
            ));
        }
        let subtasks = self.journal(|journal| journal.list_subtasks()).await?;
        let mut brief = BriefReply {
            running: 0,
            done: 0,
            waiting: 0,
        };
        for subtask in subtasks {
            let dispatch = self
                .journal(move |journal| journal.load_dispatch(subtask.dispatch))
                .await?;
            if dispatch
                .as_ref()
                .and_then(|dispatch| dispatch.claude_session.as_deref())
                != Some(claude_session)
            {
                continue;
            }
            match subtask.state {
                SubtaskState::Done => brief.done = brief.done.saturating_add(1),
                SubtaskState::WaitingOnMain { .. } => {
                    brief.waiting = brief.waiting.saturating_add(1);
                }
                state if !state.is_terminal() => {
                    brief.running = brief.running.saturating_add(1);
                }
                _ => {}
            }
        }
        Ok(brief)
    }

    async fn invite(&self, name: String, ttl_seconds: u64) -> Result<InviteReply, ServiceError> {
        let name = name.trim();
        if name.is_empty() || name.len() > 128 {
            return Err(ServiceError::Invalid(
                "node name must be between 1 and 128 characters".into(),
            ));
        }
        if ttl_seconds == 0 || ttl_seconds > MAX_INVITE_TTL_SECONDS {
            return Err(ServiceError::Invalid(format!(
                "invite TTL must be between 1 and {MAX_INVITE_TTL_SECONDS} seconds"
            )));
        }
        let mut code = [0_u8; 32];
        getrandom::fill(&mut code).map_err(|_| ServiceError::Random)?;
        let invite = Invite::new(self.public_address.clone(), self.ca.fingerprint(), code)
            .map_err(|_| ServiceError::Internal)?;
        let stored_code = BASE32_NOPAD.encode(invite.code_bytes());
        let store_code = stored_code.clone();
        let store_name = name.to_owned();
        let expires_at = Timestamp::now().checked_add(Duration::from_secs(ttl_seconds));
        self.journal(move |journal| journal.create_invite(&store_code, &store_name, expires_at))
            .await?;
        Ok(InviteReply {
            invite: invite.encode(),
            name: name.to_owned(),
            expires_in_seconds: ttl_seconds,
        })
    }

    async fn node_lifecycle(
        &self,
        name: String,
        action: NodeAction,
    ) -> Result<LocalResponseBody, ServiceError> {
        let node = self
            .journal({
                let name = name.clone();
                move |journal| journal.find_node_by_name(&name)
            })
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("node {name}")))?;
        let node_id = node.id;
        self.journal(move |journal| match action {
            NodeAction::Approve => journal.approve_node(node_id),
            NodeAction::Pause => journal.pause_node(node_id),
            NodeAction::Resume => journal.resume_node(node_id),
            NodeAction::Remove => journal.remove_node(node_id, Timestamp::now()),
        })
        .await?;
        match action {
            NodeAction::Pause => {
                self.reassign_tasks_from_node(node_id, ReassignReason::NodePaused)
                    .await?;
            }
            NodeAction::Remove => {
                self.reassign_tasks_from_node(node_id, ReassignReason::NodeRemoved)
                    .await?;
            }
            NodeAction::Approve | NodeAction::Resume => {}
        }
        Ok(LocalResponseBody::NodeChanged { name })
    }

    async fn journal<T, F>(&self, work: F) -> Result<T, ServiceError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Journal) -> Result<T, JournalError> + Send + 'static,
    {
        let journal = Arc::clone(&self.journal);
        tokio::task::spawn_blocking(move || {
            let mut journal = journal.lock().map_err(|_| ServiceError::Internal)?;
            work(&mut journal).map_err(ServiceError::Journal)
        })
        .await
        .map_err(|_| ServiceError::Worker)?
    }
}

#[derive(Debug, Clone)]
struct ResolvedSnapshot {
    base: tagteam_core::GitOid,
    repo_id: RepoId,
    excluded_paths: Vec<String>,
}

/// Resolve and validate direct daemon input. The MCP shim rejects malformed
/// schemas first, but the local socket is also a trust boundary: callers cannot
/// smuggle in a different repository, empty checks, or unbounded limits.
fn normalize_and_validate_spec(
    spec: &mut SubtaskSpec,
    repository: &Path,
) -> Result<(), ServiceError> {
    spec.title = spec.title.trim().to_owned();
    if spec.title.is_empty() {
        return Err(ServiceError::Invalid(
            "subtask title must not be empty".into(),
        ));
    }
    spec.prompt = spec.prompt.trim().to_owned();
    if spec.prompt.is_empty() {
        return Err(ServiceError::Invalid(format!(
            "subtask {:?} prompt must not be empty",
            spec.title
        )));
    }
    if spec.done_when.is_empty() {
        return Err(ServiceError::Invalid(format!(
            "subtask {:?} needs at least one done_when command",
            spec.title
        )));
    }
    for command in &mut spec.done_when {
        *command = command.trim().to_owned();
        if command.is_empty() {
            return Err(ServiceError::Invalid(format!(
                "subtask {:?} has an empty done_when command",
                spec.title
            )));
        }
    }

    spec.base_ref = spec.base_ref.trim().to_owned();
    if spec.base_ref.is_empty() {
        return Err(ServiceError::Invalid("base_ref must not be empty".into()));
    }
    for host in &mut spec.egress {
        *host = host.trim().to_owned();
        if host.is_empty() {
            return Err(ServiceError::Invalid(
                "egress hosts must not be empty".into(),
            ));
        }
    }
    spec.node_hint = spec
        .node_hint
        .take()
        .map(|hint| hint.trim().to_owned())
        .filter(|hint| !hint.is_empty());
    for (name, _) in &mut spec.needs.toolchains {
        *name = name.trim().to_owned();
        if name.is_empty() {
            return Err(ServiceError::Invalid(
                "toolchain name must not be empty".into(),
            ));
        }
    }
    if spec.needs.cores == 0 {
        return Err(ServiceError::Invalid(
            "needs.cores must be greater than zero".into(),
        ));
    }
    if spec.limits.max_turns == 0 {
        return Err(ServiceError::Invalid(
            "limits.max_turns must be greater than zero".into(),
        ));
    }
    for (field, duration) in [
        ("wall_clock", spec.limits.wall_clock),
        ("command_timeout", spec.limits.command_timeout),
        ("question_deadline", spec.limits.question_deadline),
    ] {
        if duration.is_zero() {
            return Err(ServiceError::Invalid(format!(
                "limits.{field} must be greater than zero"
            )));
        }
    }
    for (field, bytes) in [
        ("max_output", spec.limits.max_output),
        ("model_output", spec.limits.model_output),
        ("max_patch", spec.limits.max_patch),
    ] {
        if bytes.0 == 0 {
            return Err(ServiceError::Invalid(format!(
                "limits.{field} must be greater than zero"
            )));
        }
    }

    // Placement and execution always use the dispatch repository. Ignoring a
    // conflicting per-subtask path here would make the durable record lie.
    spec.repo = repository.to_path_buf();
    Ok(())
}

async fn resolve_snapshot(
    repository: Repository,
    base_ref: String,
    include_uncommitted: bool,
) -> Result<ResolvedSnapshot, ServiceError> {
    tokio::task::spawn_blocking(move || {
        if include_uncommitted {
            let snapshot = repository.snapshot(&base_ref)?;
            let repo_id = repository.repo_id(&snapshot.base.0)?;
            Ok(ResolvedSnapshot {
                base: snapshot.commit,
                repo_id,
                excluded_paths: snapshot.excluded_paths,
            })
        } else {
            let base = repository.resolve_revision(&base_ref)?;
            let repo_id = repository.repo_id(&base.0)?;
            Ok(ResolvedSnapshot {
                base,
                repo_id,
                excluded_paths: Vec::new(),
            })
        }
    })
    .await
    .map_err(|_| ServiceError::Worker)?
}

/// Produce a safe, collision-resistant review branch name. The ULID suffix is
/// unique even when two subtasks share a title, while the slug remains legible
/// in Git tooling.
fn branch_name(title: &str, id: SubtaskId) -> String {
    const MAX_SLUG_BYTES: usize = 40;
    let mut slug = String::with_capacity(MAX_SLUG_BYTES);
    let mut pending_separator = false;
    for character in title.chars() {
        if slug.len() >= MAX_SLUG_BYTES {
            break;
        }
        if character.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                if slug.len() >= MAX_SLUG_BYTES {
                    break;
                }
                slug.push('-');
            }
            if slug.len() >= MAX_SLUG_BYTES {
                break;
            }
            pending_separator = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "task" } else { slug };
    let encoded_id = id.to_string();
    let suffix = &encoded_id[encoded_id.len().saturating_sub(6)..];
    format!("tagteam/{slug}-{}", suffix.to_ascii_lowercase())
}

fn ensure_prepare_succeeded(result: &Proto::CommandResult) -> Result<(), ServiceError> {
    match result.result.as_ref() {
        Some(Proto::command_result::Result::Done(done)) if done.ok => Ok(()),
        Some(Proto::command_result::Result::Error(error)) => Err(ServiceError::Invalid(format!(
            "node rejected workspace preparation: {}",
            bounded_error(&error.message)
        ))),
        Some(_) => Err(ServiceError::Invalid(
            "node returned an unexpected result for workspace preparation".into(),
        )),
        None => Err(ServiceError::Invalid(
            "node returned no result for workspace preparation".into(),
        )),
    }
}

/// Replaces heartbeat's process-local running count with the durable account of
/// work that still holds a node slot. Heartbeats cannot know about work queued
/// between process restarts; the scheduler must use durable state instead.
fn refresh_node_running(nodes: &mut [tagteam_core::Node], tasks: &[Subtask]) {
    for node in nodes {
        let running = tasks
            .iter()
            .filter(|task| task.node == Some(node.id) && task.state.occupies_worker_slot())
            .count();
        node.capacity.running = u16::try_from(running).unwrap_or(u16::MAX);
    }
}

fn bounded_error(message: &str) -> String {
    message.chars().take(512).collect()
}

#[derive(Clone, Copy)]
enum NodeAction {
    Approve,
    Pause,
    Resume,
    Remove,
}

fn status_for(
    task: &Subtask,
    events: &[Event],
    question: Option<&crate::journal::StoredQuestion>,
) -> SubtaskStatus {
    let last_progress = events
        .iter()
        .filter(|event| event.subtask == task.id)
        .filter_map(|event| match &event.kind {
            EventKind::Progress { note, .. } => Some(note.clone()),
            EventKind::ToolRan { tool, summary, .. } => Some(format!("{tool}: {summary}")),
            EventKind::Placed { node } => Some(format!("placed on {node}")),
            EventKind::Finished(result) => Some(result.summary.clone()),
            EventKind::Failed { reason } => Some(format!("failed: {reason:?}")),
            EventKind::Cancelled => Some("cancelled".into()),
            _ => None,
        })
        .next_back();
    let open_question = match &task.state {
        SubtaskState::WaitingOnMain { question } => Some(question.to_string()),
        _ => None,
    };
    SubtaskStatus {
        id: task.id,
        title: task.spec.title.clone(),
        state: state_label(&task.state).into(),
        node: task.node.map(|node| node.to_string()),
        usage: task.usage,
        last_progress,
        open_question,
        open_question_text: question.map(|question| question.question.text.clone()),
        question_deadline: question.map(|question| question.question.deadline),
        branch: Some(task.branch.clone()),
    }
}

fn state_label(state: &SubtaskState) -> &'static str {
    match state {
        SubtaskState::Queued => "queued",
        SubtaskState::Placing => "placing",
        SubtaskState::Preparing => "preparing",
        SubtaskState::Running => "running",
        SubtaskState::WaitingOnMain { .. } => "waiting_on_main",
        SubtaskState::Reassigning { .. } => "reassigning",
        SubtaskState::Collecting => "collecting",
        SubtaskState::Done => "done",
        SubtaskState::Failed { .. } => "failed",
        SubtaskState::Cancelled => "cancelled",
    }
}

fn render_event(event: &Event) -> String {
    format!(
        "{} [{}] {}",
        event.at.0,
        event.subtask,
        match &event.kind {
            EventKind::Placed { node } => format!("placed on {node}"),
            EventKind::Progress { note, percent } => percent
                .map(|percent| format!("{percent:>3}% {note}"))
                .unwrap_or_else(|| note.clone()),
            EventKind::ToolRan {
                tool,
                summary,
                ok,
                took,
            } => format!(
                "{} {tool}: {summary} ({:.1?})",
                if *ok { "ok" } else { "error" },
                took
            ),
            EventKind::Question(question) => format!("question: {}", question.text),
            EventKind::Answered { question, .. } => format!("answered {question}"),
            EventKind::Steered { text } => format!("steered: {text}"),
            EventKind::Reassigned { from, reason } =>
                format!("reassigned from {from} ({reason:?})"),
            EventKind::ReassignedTo { to } => format!("reassigned to {to}"),
            EventKind::Finished(result) => format!("finished: {}", result.summary),
            EventKind::Failed { reason } => format!("failed: {reason:?}"),
            EventKind::Cancelled => "cancelled".into(),
        }
    )
}

#[derive(Debug)]
struct RemoteOutcome {
    text: String,
    is_error: bool,
}

#[derive(Debug)]
struct FinishRequest {
    outcome: Outcome,
    summary: String,
    assumptions: Vec<Assumption>,
}

fn runner_system_append(task: &Subtask) -> String {
    let checks = task
        .spec
        .done_when
        .iter()
        .map(|command| format!("- {command}"))
        .collect::<Vec<_>>()
        .join("\n");
    bounded_text(
        &format!(
            "You are executing one Tag Team subtask in a remote checkout.\n\n\
             Task: {}\n\n\
             Use only the mcp__tagteam__ tools. Every read, write, edit, search, and command \
             must go through those tools; local shell and file tools are unavailable and are \
             denied. Do not attempt to access credentials, user configuration, or paths outside \
             the checkout. Keep commands bounded and report meaningful progress.\n\n\
             The hub will authoritatively re-run these done_when checks before accepting a \
             completed or no_change result:\n{checks}\n\n\
             When the work is ready, call mcp__tagteam__Finish with a concise outcome, summary, \
             and any assumptions. Do not claim a check passed unless you actually observed it.",
            task.spec.title,
        ),
        8_000,
    )
}

fn allowed_runner_tool(tool: &str) -> bool {
    taskteam_runner_tools().contains(&tool)
}

fn taskteam_runner_tools() -> &'static [&'static str] {
    &[
        "mcp__tagteam__Bash",
        "mcp__tagteam__Read",
        "mcp__tagteam__Write",
        "mcp__tagteam__Edit",
        "mcp__tagteam__Glob",
        "mcp__tagteam__Grep",
        "mcp__tagteam__AskMain",
        "mcp__tagteam__ReportProgress",
        "mcp__tagteam__Finish",
    ]
}

fn remote_tool_name(call: &RemoteCall) -> &'static str {
    match call {
        RemoteCall::Bash { .. } => "Bash",
        RemoteCall::Read { .. } => "Read",
        RemoteCall::Write { .. } => "Write",
        RemoteCall::Edit { .. } => "Edit",
        RemoteCall::Glob { .. } => "Glob",
        RemoteCall::Grep { .. } => "Grep",
        RemoteCall::AskMain { .. } => "AskMain",
        RemoteCall::ReportProgress { .. } => "ReportProgress",
        RemoteCall::Finish { .. } => "Finish",
    }
}

fn remote_command(
    task: &Subtask,
    call_id: &str,
    call: &RemoteCall,
) -> Result<Proto::Command, ServiceError> {
    if call_id.trim().is_empty() {
        return Err(ServiceError::Invalid("runner call ID is empty".into()));
    }
    let op = match call {
        RemoteCall::Bash {
            command,
            timeout_ms,
        } => {
            let timeout = timeout_ms
                .filter(|value| *value > 0)
                .unwrap_or(u64::from(command_timeout_ms(&task.spec.limits)))
                .min(u64::from(command_timeout_ms(&task.spec.limits)));
            Proto::command::Op::Exec(Proto::Exec {
                argv: vec!["bash".into(), "-lc".into(), command.clone()],
                cwd: String::new(),
                env: HashMap::new(),
                stdin: Vec::new(),
                timeout_ms: u32::try_from(timeout).unwrap_or(u32::MAX),
                output_cap: output_cap(&task.spec.limits),
            })
        }
        RemoteCall::Read {
            path,
            offset,
            limit,
        } => Proto::command::Op::Read(Proto::ReadFiles {
            files: vec![Proto::FileRange {
                path: path_text(path)?,
                offset: offset.unwrap_or(0),
                limit: limit.unwrap_or(0),
            }],
        }),
        RemoteCall::Write { path, content } => {
            if content.len() as u64 > task.spec.limits.max_patch.0 {
                return Err(ServiceError::Invalid(
                    "runner write exceeds the subtask patch limit".into(),
                ));
            }
            Proto::command::Op::Write(Proto::WriteFiles {
                files: vec![Proto::FileBody {
                    path: path_text(path)?,
                    data: content.as_bytes().to_vec(),
                    expected_sha256: None,
                }],
            })
        }
        RemoteCall::Edit {
            path,
            old,
            new,
            replace_all,
        } => {
            if new.len() as u64 > task.spec.limits.max_patch.0 {
                return Err(ServiceError::Invalid(
                    "runner edit exceeds the subtask patch limit".into(),
                ));
            }
            Proto::command::Op::Edit(Proto::EditFile {
                path: path_text(path)?,
                old: old.clone(),
                new: new.clone(),
                replace_all: *replace_all,
            })
        }
        RemoteCall::Glob { pattern, path } => Proto::command::Op::Glob(Proto::Glob {
            pattern: pattern.clone(),
            cwd: path
                .as_deref()
                .map(path_text)
                .transpose()?
                .unwrap_or_default(),
            limit: 10_000,
        }),
        RemoteCall::Grep {
            pattern,
            path,
            glob,
            ignore_case,
            context,
            limit,
        } => Proto::command::Op::Grep(Proto::Grep {
            pattern: pattern.clone(),
            path: path
                .as_deref()
                .map(path_text)
                .transpose()?
                .unwrap_or_default(),
            glob: glob.clone().unwrap_or_default(),
            ignore_case: *ignore_case,
            context: context.unwrap_or(3).min(100),
            limit: limit.unwrap_or(200).min(10_000),
        }),
        RemoteCall::ReportProgress { .. }
        | RemoteCall::Finish { .. }
        | RemoteCall::AskMain { .. } => {
            return Err(ServiceError::Invalid(
                "runner control call must not be sent to the node".into(),
            ));
        }
    };
    Ok(Proto::Command {
        command_id: format!("remote-{}-a{}-{call_id}", task.id, task.attempt),
        subtask_id: task.id.to_string(),
        workspace: task.id.to_string(),
        op: Some(op),
    })
}

fn render_remote_result(
    result: &Proto::CommandResult,
    task: &Subtask,
) -> Result<RemoteOutcome, ServiceError> {
    let (text, is_error) = match result.result.as_ref() {
        Some(Proto::command_result::Result::Exec(exec)) => {
            let report = ExecReport {
                exit_code: Some(exec.exit_code),
                signal: (exec.signal != 0).then_some(exec.signal),
                timed_out: exec.timed_out,
                killed: exec.killed,
                stdout_tail: exec.stdout_tail.clone(),
                stderr_tail: exec.stderr_tail.clone(),
                truncated: exec.truncated,
                took: Duration::from_millis(exec.took_ms),
            };
            let failed = exec.timed_out || exec.killed || exec.signal != 0 || exec.exit_code != 0;
            (report.render(), failed)
        }
        Some(Proto::command_result::Result::Read(read)) => {
            let mut text = String::new();
            for file in &read.files {
                text.push_str(&format!("{}\n", file.path));
                text.push_str(&String::from_utf8_lossy(&file.data));
                if !text.ends_with('\n') {
                    text.push('\n');
                }
            }
            (text, false)
        }
        Some(Proto::command_result::Result::Written(written)) => (
            format!(
                "wrote {} file(s): {}",
                written.paths.len(),
                written.paths.join(", ")
            ),
            false,
        ),
        Some(Proto::command_result::Result::Edited(edited)) => (
            format!(
                "edited {} ({} replacement(s))",
                edited.path, edited.replacements
            ),
            false,
        ),
        Some(Proto::command_result::Result::Matches(matches)) => (
            format!(
                "{}{}",
                matches.lines.join("\n"),
                if matches.truncated {
                    "\n[matches truncated]"
                } else {
                    ""
                }
            ),
            false,
        ),
        Some(Proto::command_result::Result::Error(error)) => (bounded_error(&error.message), true),
        Some(Proto::command_result::Result::Committed(committed)) => {
            (format!("committed {}", committed.oid), false)
        }
        Some(Proto::command_result::Result::Collected(blob)) => (
            format!("collected result bundle ({} bytes)", blob.size),
            false,
        ),
        Some(Proto::command_result::Result::Done(done)) => {
            (format!("operation completed: {}", done.ok), !done.ok)
        }
        None => {
            return Err(ServiceError::Invalid(
                "node returned no command result".into(),
            ));
        }
    };
    Ok(RemoteOutcome {
        text: model_bounded_text(text, task.spec.limits.model_output),
        is_error,
    })
}

fn path_text(path: &Path) -> Result<String, ServiceError> {
    path.to_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ServiceError::Invalid("runner supplied an invalid path".into()))
}

fn command_timeout_ms(limits: &tagteam_core::Limits) -> u32 {
    u32::try_from(limits.command_timeout.as_millis()).unwrap_or(u32::MAX)
}

fn output_cap(limits: &tagteam_core::Limits) -> u32 {
    u32::try_from(limits.max_output.0).unwrap_or(u32::MAX)
}

fn command_wait(task: &Subtask) -> Duration {
    task.spec
        .limits
        .command_timeout
        .saturating_add(Duration::from_secs(30))
}

fn render_exec_tail(exec: &Proto::ExecDone) -> String {
    ExecReport {
        exit_code: Some(exec.exit_code),
        signal: (exec.signal != 0).then_some(exec.signal),
        timed_out: exec.timed_out,
        killed: exec.killed,
        stdout_tail: exec.stdout_tail.clone(),
        stderr_tail: exec.stderr_tail.clone(),
        truncated: exec.truncated,
        took: Duration::from_millis(exec.took_ms),
    }
    .render()
}

fn model_bounded_text(text: String, cap: tagteam_core::Bytes) -> String {
    let cap = usize::try_from(cap.0).unwrap_or(usize::MAX);
    let (bytes, _) = tail_middle(text.as_bytes(), tagteam_core::Bytes(cap as u64));
    String::from_utf8_lossy(&bytes).into_owned()
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests;
