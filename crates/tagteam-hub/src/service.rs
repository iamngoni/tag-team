//! Application service behind the local daemon API.
//!
//! The service owns the durable journal handle shared by CLI and MCP requests,
//! while node transport remains behind [`crate::nodes::NodePlane`]. Requests are
//! validated at this boundary; callers never receive raw SQLite or filesystem errors.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use data_encoding::BASE32_NOPAD;
use tagteam_core::{
    Dispatch, DispatchId, Event, EventKind, MainSessionId, RepoId, Subtask, SubtaskId, SubtaskSpec,
    SubtaskState, Timestamp,
    local_api::{
        BriefReply, CollectReply, DispatchInput, DispatchReply, InviteReply,
        LOCAL_PROTOCOL_VERSION, LocalRequest, LocalResponseBody, LogsReply, NodeView, StatusReply,
        SubtaskStatus, WatchScope,
    },
    schedule::{Placement, place},
    state::SubtaskInput,
};
use tagteam_git::Repository;
use tagteam_pki::{HubCa, Invite};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    config::{DataPaths, HubConfig},
    journal::{EventFilter, EventScope, Journal, JournalError},
    nodes::{NodePlane, NodePlaneError},
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
}

impl HubService {
    /// Opens the durable data directory, journal and shared CA-backed node plane.
    pub async fn open(config: &HubConfig, ca: Arc<HubCa>) -> Result<Self, ServiceError> {
        let paths = config.prepare_data_dir()?;
        let journal_path = paths.journal.clone();
        let journal = tokio::task::spawn_blocking(move || Journal::open(journal_path))
            .await
            .map_err(|_| ServiceError::Worker)??;
        let nodes = NodePlane::with_ca(&paths.journal, &paths.blobs, Arc::clone(&ca)).await?;
        Ok(Self {
            journal: Arc::new(Mutex::new(journal)),
            nodes,
            ca,
            public_address: config.public_address.clone(),
            paths,
            attachments: Arc::new(AsyncMutex::new(HashMap::new())),
            claude_sessions: Arc::new(AsyncMutex::new(HashMap::new())),
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
            LocalRequest::Status { ids } => self.status(ids).await.map(LocalResponseBody::Status),
            LocalRequest::Inbox {
                since_seq,
                wait_ms,
                kinds,
                all: _,
            } => self
                .inbox(since_seq, wait_ms, kinds.as_deref())
                .await
                .map(LocalResponseBody::Events),
            LocalRequest::Watch { .. } => Err(ServiceError::Unsupported(
                "watch is a streaming request".into(),
            )),
            LocalRequest::Logs { subtask, tail } => {
                self.logs(subtask, tail).await.map(LocalResponseBody::Logs)
            }
            LocalRequest::Cancel { subtask, reason } => {
                self.cancel(subtask, reason).await?;
                Ok(LocalResponseBody::Cancelled { subtask })
            }
            LocalRequest::Collect { subtask, worktree } => self
                .collect(subtask, worktree)
                .await
                .map(LocalResponseBody::Collected),
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
    ) -> Result<Vec<Event>, ServiceError> {
        let wait_ms = wait_ms.min(50_000);
        let kinds = kinds.unwrap_or_default().to_vec();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let kinds = kinds.clone();
            let events = self
                .journal(move |journal| {
                    let kind_refs = kinds.iter().map(String::as_str).collect::<Vec<_>>();
                    journal.events(EventFilter {
                        since_seq,
                        kinds: &kind_refs,
                        scope: EventScope::All,
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

        let mut nodes = self.journal(|journal| journal.list_nodes()).await?;
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
            let placement = place(&spec, &snapshot.repo_id, &nodes, sibling_scope, created);
            let transition = match &placement {
                Placement::Placed(node) => {
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

    async fn status(&self, ids: Option<Vec<SubtaskId>>) -> Result<StatusReply, ServiceError> {
        let wanted = ids.map(|ids| ids.into_iter().collect::<std::collections::HashSet<_>>());
        let subtasks = self.journal(|journal| journal.list_subtasks()).await?;
        let events = self
            .journal(|journal| journal.events(EventFilter::default()))
            .await?;
        let subtasks = subtasks
            .into_iter()
            .filter(|subtask| {
                wanted
                    .as_ref()
                    .is_none_or(|wanted| wanted.contains(&subtask.id))
            })
            .map(|subtask| status_for(&subtask, &events))
            .collect();
        Ok(StatusReply { subtasks })
    }

    async fn logs(&self, subtask: SubtaskId, tail: u32) -> Result<LogsReply, ServiceError> {
        if self
            .journal(move |journal| journal.load_subtask(subtask))
            .await?
            .is_none()
        {
            return Err(ServiceError::NotFound(format!("subtask {subtask}")));
        }
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

    async fn cancel(&self, subtask: SubtaskId, reason: String) -> Result<(), ServiceError> {
        if reason.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "cancel reason must not be empty".into(),
            ));
        }
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
        Ok(())
    }

    async fn collect(
        &self,
        subtask: SubtaskId,
        worktree: bool,
    ) -> Result<CollectReply, ServiceError> {
        let task = self
            .journal(move |journal| journal.load_subtask(subtask))
            .await?
            .ok_or_else(|| ServiceError::NotFound(format!("subtask {subtask}")))?;
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
        self.journal(move |journal| match action {
            NodeAction::Approve => journal.approve_node(node.id),
            NodeAction::Pause => journal.pause_node(node.id),
            NodeAction::Resume => journal.resume_node(node.id),
            NodeAction::Remove => journal.remove_node(node.id, Timestamp::now()),
        })
        .await?;
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

#[derive(Clone, Copy)]
enum NodeAction {
    Approve,
    Pause,
    Resume,
    Remove,
}

fn status_for(task: &Subtask, events: &[Event]) -> SubtaskStatus {
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

#[cfg(test)]
mod tests;
