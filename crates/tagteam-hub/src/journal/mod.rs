//! Durable SQLite journal for hub state and ordered events.
//!
//! This module owns persistence only. Subtask transitions always pass through
//! `tagteam_core::state::step`, and the resulting state/event share a transaction.

use std::{fs, path::Path, str::FromStr};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tagteam_core::{
    CachedRepo, Capacity, Dispatch, DispatchId, Event, EventKind, GitOid, Node, NodeId, NodePolicy,
    NodeState, OwnerPresence, Platform, RepoId, ResultSummary, Subtask, SubtaskId, SubtaskState,
    Timestamp, Toolchain, Usage,
    state::{self, IllegalTransition, SubtaskInput},
};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 1;

type ResultRow = (
    String,
    String,
    Option<Vec<u8>>,
    Option<i64>,
    Option<String>,
    String,
    String,
    String,
);

#[derive(Debug, Error)]
/// Failures returned by journal reads and writes.
pub enum JournalError {
    #[error("journal filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite journal error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("journal serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("illegal subtask transition: {0}")]
    IllegalTransition(Box<IllegalTransition>),
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("invite code is invalid")]
    InvalidInviteCode,
    #[error("invite has expired")]
    InviteExpired,
    #[error("invite has already been used")]
    InviteAlreadyUsed,
    #[error("node name already exists")]
    NodeNameAlreadyExists,
    #[error("enrollment node must be in the enrolled state")]
    InvalidEnrollmentState,
    #[error("idempotency key conflicts with an existing record: {0}")]
    IdempotencyConflict(String),
    #[error("unsupported journal schema version {0}")]
    UnsupportedSchema(i64),
    #[error("integer value is outside SQLite's supported range")]
    IntegerRange,
}

/// A synchronous SQLite journal. Callers that need async integration should
/// place it behind `spawn_blocking`.
pub struct Journal {
    connection: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Invite metadata returned after hashing the supplied enrollment code.
pub struct Invite {
    pub name: String,
    pub expires_at: Timestamp,
    pub used_at: Option<Timestamp>,
    pub node_id: Option<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A durable node operation and its optional completion result.
pub struct Command {
    pub id: String,
    pub node_id: NodeId,
    pub subtask_id: SubtaskId,
    pub operation: Value,
    pub state: String,
    pub result: Option<Value>,
    pub sent_at: Option<Timestamp>,
    pub done_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// The committed state and ordered event produced by one transition.
pub struct TransitionResult {
    pub state: SubtaskState,
    pub event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// Optional event scope for one subtask or a watch set.
pub enum EventScope<'a> {
    #[default]
    All,
    Subtask(SubtaskId),
    Watch(&'a [SubtaskId]),
}

#[derive(Debug, Clone, Copy, Default)]
/// Cursor, kind, scope, and result limit for journal event reads.
pub struct EventFilter<'a> {
    pub since_seq: u64,
    /// Snake-case `EventKind` names, such as `progress` or `finished`.
    pub kinds: &'a [&'a str],
    pub scope: EventScope<'a>,
    pub limit: usize,
}

impl Journal {
    /// Opens a persistent database, creating its parent directory and applying
    /// any pending migrations exactly once.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(JournalError::UnsupportedSchema(version));
        }
        if version < 1 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(include_str!("../../migrations/0001_initial.sql"))?;
            transaction.pragma_update(None, "user_version", 1)?;
            transaction.commit()?;
        }
        Ok(Self { connection })
    }

    /// Persists an enrolled node. Repeating the same node ID is idempotent;
    /// reusing its ID for different node data is an error.
    pub fn create_node(&mut self, node: &Node) -> Result<(), JournalError> {
        let existing = self.load_node(node.id)?;
        if let Some(existing) = existing {
            if existing.name == node.name
                && existing.platform == node.platform
                && existing.cert_sha256 == node.cert_sha256
            {
                return Ok(());
            }
            return Err(JournalError::IdempotencyConflict(node.id.to_string()));
        }
        self.connection.execute(
            "INSERT INTO nodes (id,name,os,arch,os_version,state,trust,presence,cert_sha256,capacity,toolchains,repos,policy,last_seen_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                node.id.to_string(), node.name, enum_text(&node.platform.os)?,
                enum_text(&node.platform.arch)?, node.platform.os_version,
                enum_text(&node.state)?, enum_text(&node.trust)?,
                serde_json::to_string(&node.presence)?, node.cert_sha256.as_slice(),
                serde_json::to_string(&node.capacity)?, serde_json::to_string(&node.toolchains)?,
                serde_json::to_string(&node.repo_heads)?, serde_json::to_string(&node.policy)?, node.last_seen.0,
            ],
        )?;
        Ok(())
    }

    /// Consumes an unused invite and inserts its enrolled node in one
    /// transaction. Any insert or constraint failure rolls the invite spend
    /// back with the node creation.
    pub fn enroll_node(
        &mut self,
        code: &str,
        now: Timestamp,
        node: &Node,
    ) -> Result<(), JournalError> {
        if node.state != NodeState::Enrolled {
            return Err(JournalError::InvalidEnrollmentState);
        }
        if code.is_empty() {
            return Err(JournalError::InvalidInviteCode);
        }

        let digest = Sha256::digest(code.as_bytes());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let invite: Option<(i64, Option<i64>)> = transaction
            .query_row(
                "SELECT expires_at,used_at FROM invites WHERE code_sha256=?1",
                [digest.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((expires_at, used_at)) = invite else {
            return Err(JournalError::InvalidInviteCode);
        };
        if used_at.is_some() {
            return Err(JournalError::InviteAlreadyUsed);
        }
        if expires_at <= now.0 {
            return Err(JournalError::InviteExpired);
        }

        let name_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE name=?1)",
            [&node.name],
            |row| row.get(0),
        )?;
        if name_exists {
            return Err(JournalError::NodeNameAlreadyExists);
        }

        transaction.execute(
            "INSERT INTO nodes (id,name,os,arch,os_version,state,trust,presence,cert_sha256,capacity,toolchains,repos,policy,last_seen_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                node.id.to_string(), node.name, enum_text(&node.platform.os)?,
                enum_text(&node.platform.arch)?, node.platform.os_version,
                enum_text(&node.state)?, enum_text(&node.trust)?,
                serde_json::to_string(&node.presence)?, node.cert_sha256.as_slice(),
                serde_json::to_string(&node.capacity)?, serde_json::to_string(&node.toolchains)?,
                serde_json::to_string(&node.repo_heads)?, serde_json::to_string(&node.policy)?, node.last_seen.0,
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE invites SET used_at=?2,node_id=?3 WHERE code_sha256=?1 AND used_at IS NULL AND expires_at>?2",
            params![digest.as_slice(), now.0, node.id.to_string()],
        )?;
        if changed != 1 {
            return Err(JournalError::InviteAlreadyUsed);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Loads the durable node record, including presence and trust.
    pub fn load_node(&self, id: NodeId) -> Result<Option<Node>, JournalError> {
        type RawNode = (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            Vec<u8>,
            String,
            String,
            String,
            String,
            i64,
            String,
        );
        let raw: Option<RawNode> = self.connection.query_row(
            "SELECT id,name,os,arch,os_version,state,trust,cert_sha256,capacity,toolchains,repos,policy,last_seen_at,presence FROM nodes WHERE id=?1",
            [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?,row.get(9)?,row.get(10)?,row.get(11)?,row.get(12)?,row.get(13)?)),
        ).optional()?;
        raw.map(
            |(
                id,
                name,
                os,
                arch,
                os_version,
                state,
                trust,
                cert,
                capacity,
                toolchains,
                repos,
                policy,
                last_seen,
                presence,
            )| {
                let cert_sha256: [u8; 32] = cert.try_into().map_err(|_| {
                    JournalError::IdempotencyConflict("invalid stored certificate digest".into())
                })?;
                Ok(Node {
                    id: NodeId::from_str(&id)
                        .map_err(|e| JournalError::IdempotencyConflict(e.to_string()))?,
                    name,
                    platform: Platform {
                        os: serde_json::from_value(Value::String(os))?,
                        arch: serde_json::from_value(Value::String(arch))?,
                        os_version,
                    },
                    state: serde_json::from_value(Value::String(state))?,
                    trust: serde_json::from_value(Value::String(trust))?,
                    cert_sha256,
                    capacity: parse_json(&capacity)?,
                    toolchains: parse_json::<Vec<Toolchain>>(&toolchains)?,
                    repo_heads: parse_json::<Vec<tagteam_core::CachedRepo>>(&repos)?,
                    policy: parse_json::<NodePolicy>(&policy)?,
                    presence: parse_json(&presence)?,
                    last_seen: Timestamp(last_seen),
                })
            },
        )
        .transpose()
    }

    /// Finds an active durable node by the SHA-256 fingerprint from its TLS
    /// client certificate. Blob RPCs carry no node ID, so this lookup keeps
    /// certificate authorization inside the journal boundary.
    pub fn load_node_by_certificate_sha256(
        &self,
        fingerprint: &[u8; 32],
    ) -> Result<Option<Node>, JournalError> {
        let encoded_id: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM nodes WHERE cert_sha256=?1 AND state<>'removed' AND revoked_at IS NULL LIMIT 1",
                [fingerprint.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(encoded_id) = encoded_id else {
            return Ok(None);
        };
        let id = NodeId::from_str(&encoded_id)
            .map_err(|error| JournalError::IdempotencyConflict(error.to_string()))?;
        let node = self.load_node(id)?;
        Ok(node.filter(|node| !node.state.is_removed()))
    }

    /// Activates an enrolled node; repeating approval is safe.
    pub fn approve_node(&mut self, id: NodeId) -> Result<(), JournalError> {
        self.set_node_state(id, NodeState::Active, &[NodeState::Enrolled])
    }

    /// Pauses an active or owner-busy node; repeating the operation is safe.
    pub fn pause_node(&mut self, id: NodeId) -> Result<(), JournalError> {
        self.set_node_state(
            id,
            NodeState::Paused,
            &[NodeState::Active, NodeState::OwnerBusy],
        )
    }

    /// Resumes a paused node; repeating the operation is safe.
    pub fn resume_node(&mut self, id: NodeId) -> Result<(), JournalError> {
        self.set_node_state(id, NodeState::Active, &[NodeState::Paused])
    }

    /// Revokes and marks a node removed without deleting its durable record.
    pub fn remove_node(&mut self, id: NodeId, at: Timestamp) -> Result<(), JournalError> {
        let changed = self.connection.execute(
            "UPDATE nodes SET state='removed', revoked_at=COALESCE(revoked_at,?2) WHERE id=?1",
            params![id.to_string(), at.0],
        )?;
        if changed == 0 {
            return Err(JournalError::NotFound(format!("node {id}")));
        }
        Ok(())
    }

    /// Updates heartbeat-owned fields without changing durable trust, policy,
    /// or approval state. Removed and revoked nodes cannot heartbeat.
    pub fn update_node_heartbeat(
        &mut self,
        id: NodeId,
        capacity: &Capacity,
        toolchains: &[Toolchain],
        repo_heads: &[CachedRepo],
        presence: OwnerPresence,
        last_seen: Timestamp,
    ) -> Result<(), JournalError> {
        let changed = self.connection.execute(
            "UPDATE nodes SET capacity=?2,toolchains=?3,repos=?4,presence=?5,last_seen_at=MAX(COALESCE(last_seen_at,?6),?6) \
             WHERE id=?1 AND state<>'removed' AND revoked_at IS NULL",
            params![
                id.to_string(),
                serde_json::to_string(capacity)?,
                serde_json::to_string(toolchains)?,
                serde_json::to_string(repo_heads)?,
                serde_json::to_string(&presence)?,
                last_seen.0,
            ],
        )?;
        if changed == 0 {
            return Err(JournalError::NotFound(format!("node {id}")));
        }
        Ok(())
    }

    fn set_node_state(
        &mut self,
        id: NodeId,
        state: NodeState,
        from: &[NodeState],
    ) -> Result<(), JournalError> {
        let current = self
            .load_node(id)?
            .ok_or_else(|| JournalError::NotFound(format!("node {id}")))?;
        if current.state == state {
            return Ok(());
        }
        if !from.contains(&current.state) {
            return Err(JournalError::IdempotencyConflict(format!(
                "node {id} state {:?}",
                current.state
            )));
        }
        self.connection.execute(
            "UPDATE nodes SET state=?2 WHERE id=?1",
            params![id.to_string(), enum_text(&state)?],
        )?;
        Ok(())
    }

    /// Stores an invite using only the SHA-256 digest of its raw code.
    pub fn create_invite(
        &mut self,
        code: &str,
        name: &str,
        expires_at: Timestamp,
    ) -> Result<(), JournalError> {
        let digest = Sha256::digest(code.as_bytes());
        let existing: Option<(String, i64)> = self
            .connection
            .query_row(
                "SELECT name,expires_at FROM invites WHERE code_sha256=?1",
                [digest.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((old_name, old_expiry)) = existing {
            if old_name == name && old_expiry == expires_at.0 {
                return Ok(());
            }
            return Err(JournalError::IdempotencyConflict("invite code".into()));
        }
        self.connection.execute(
            "INSERT INTO invites(code_sha256,name,expires_at) VALUES (?1,?2,?3)",
            params![digest.as_slice(), name, expires_at.0],
        )?;
        Ok(())
    }

    /// Finds invite metadata by hashing the supplied raw code.
    pub fn load_invite(&self, code: &str) -> Result<Option<Invite>, JournalError> {
        let digest = Sha256::digest(code.as_bytes());
        self.connection
            .query_row(
                "SELECT name,expires_at,used_at,node_id FROM invites WHERE code_sha256=?1",
                [digest.as_slice()],
                |row| {
                    let id: Option<String> = row.get(3)?;
                    Ok(Invite {
                        name: row.get(0)?,
                        expires_at: Timestamp(row.get(1)?),
                        used_at: row.get::<_, Option<i64>>(2)?.map(Timestamp),
                        node_id: id.map(|s| NodeId::from_str(&s)).transpose().map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Atomically consumes a valid invite for one enrolled node.
    /// Repeated spends always return `InviteAlreadyUsed`; command replay belongs
    /// to the caller's command ID, not to the one-time enrollment code.
    pub fn spend_invite(
        &mut self,
        code: &str,
        now: Timestamp,
        node_id: NodeId,
    ) -> Result<Invite, JournalError> {
        let digest = Sha256::digest(code.as_bytes());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, i64, Option<i64>)> = transaction
            .query_row(
                "SELECT name,expires_at,used_at FROM invites WHERE code_sha256=?1",
                [digest.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((name, expires_at, used_at)) = existing else {
            return Err(JournalError::InvalidInviteCode);
        };
        if used_at.is_some() {
            return Err(JournalError::InviteAlreadyUsed);
        }
        if expires_at <= now.0 {
            return Err(JournalError::InviteExpired);
        }
        let changed = transaction.execute(
            "UPDATE invites SET used_at=?2,node_id=?3 WHERE code_sha256=?1 AND used_at IS NULL",
            params![digest.as_slice(), now.0, node_id.to_string()],
        )?;
        if changed != 1 {
            return Err(JournalError::InviteAlreadyUsed);
        }
        transaction.commit()?;
        Ok(Invite {
            name,
            expires_at: Timestamp(expires_at),
            used_at: Some(now),
            node_id: Some(node_id),
        })
    }

    /// Stores a dispatch idempotently by dispatch ID and immutable payload.
    pub fn create_dispatch(&mut self, dispatch: &Dispatch) -> Result<(), JournalError> {
        let existing: Option<(String, Option<String>, String, i64)> = self
            .connection
            .query_row(
                "SELECT main_session,claude_session,repo,created_at FROM dispatches WHERE id=?1",
                [dispatch.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((session, claude, repo, created)) = existing {
            if session == dispatch.main_session.to_string()
                && claude == dispatch.claude_session
                && repo == dispatch.repo.to_string_lossy()
                && created == dispatch.created.0
            {
                return Ok(());
            }
            return Err(JournalError::IdempotencyConflict(dispatch.id.to_string()));
        }
        self.connection.execute("INSERT INTO dispatches(id,main_session,claude_session,repo,created_at) VALUES (?1,?2,?3,?4,?5)", params![dispatch.id.to_string(),dispatch.main_session.to_string(),dispatch.claude_session,dispatch.repo.to_string_lossy(),dispatch.created.0])?;
        Ok(())
    }

    /// Stores a new queued work item idempotently by subtask ID.
    pub fn create_subtask(&mut self, subtask: &Subtask) -> Result<(), JournalError> {
        let existing: Option<(String, String, String, String, String, i64, i64)> = self
            .connection
            .query_row(
                "SELECT dispatch_id,spec,base,repo_id,branch,attempt,created_at FROM subtasks WHERE id=?1",
                [subtask.id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        if let Some((dispatch, spec, base, repo_id, branch, attempt, created_at)) = existing {
            if dispatch == subtask.dispatch.to_string()
                && serde_json::from_str::<Value>(&spec)? == serde_json::to_value(&subtask.spec)?
                && base == subtask.base.0
                && repo_id == subtask.repo_id.0.0
                && branch == subtask.branch
                && attempt == i64::from(subtask.attempt)
                && created_at == subtask.created.0
            {
                return Ok(());
            }
            return Err(JournalError::IdempotencyConflict(subtask.id.to_string()));
        }
        self.connection.execute(
            "INSERT INTO subtasks(id,dispatch_id,title,spec,state,node_id,attempt,base,repo_id,branch,runner_session,created_at,started_at,finished_at,usage) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![subtask.id.to_string(),subtask.dispatch.to_string(),subtask.spec.title,serde_json::to_string(&subtask.spec)?,serde_json::to_string(&subtask.state)?,subtask.node.map(|id| id.to_string()),i64::from(subtask.attempt),subtask.base.0,subtask.repo_id.0.0,subtask.branch,subtask.runner_session,subtask.created.0,subtask.started.map(|t|t.0),subtask.finished.map(|t|t.0),serde_json::to_string(&subtask.usage)?],
        )?;
        Ok(())
    }

    /// Applies the one authoritative state transition and commits its event
    /// atomically. Rejected transitions leave both tables untouched.
    pub fn transition(
        &mut self,
        id: SubtaskId,
        input: SubtaskInput,
        at: Timestamp,
    ) -> Result<TransitionResult, JournalError> {
        let transaction = self.connection.transaction()?;
        let current_json: String = transaction
            .query_row(
                "SELECT state FROM subtasks WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| JournalError::NotFound(format!("subtask {id}")))?;
        let current: SubtaskState = serde_json::from_str(&current_json)?;
        let assigned_node = match &input {
            SubtaskInput::Placed { node } => Some(node.to_string()),
            _ => None,
        };
        let runner_session = match &input {
            SubtaskInput::RunnerStarted { session } => Some(session.clone()),
            _ => None,
        };
        let started = runner_session.is_some();
        let result = match &input {
            SubtaskInput::Finished { result } | SubtaskInput::Landed { result } => {
                Some(result.clone())
            }
            _ => None,
        };
        let (next, event_kind) =
            state::step(&current, input).map_err(JournalError::IllegalTransition)?;
        transaction.execute(
            "UPDATE subtasks SET state=?2,node_id=COALESCE(?3,node_id),runner_session=COALESCE(?4,runner_session),started_at=CASE WHEN ?5 THEN COALESCE(started_at,?6) ELSE started_at END,finished_at=CASE WHEN ?7 THEN ?6 ELSE finished_at END WHERE id=?1",
            params![
                id.to_string(),
                serde_json::to_string(&next)?,
                assigned_node,
                runner_session,
                started,
                at.0,
                next.is_terminal(),
            ],
        )?;
        if let Some(result) = result {
            store_result(&transaction, id, &result)?;
        }
        let seq = insert_event(&transaction, id, at, &event_kind)?;
        transaction.commit()?;
        Ok(TransitionResult {
            state: next,
            event: Event {
                seq,
                subtask: id,
                at,
                kind: event_kind,
            },
        })
    }

    /// Reads a subtask's current state without changing it.
    pub fn load_subtask_state(&self, id: SubtaskId) -> Result<Option<SubtaskState>, JournalError> {
        let encoded: Option<String> = self
            .connection
            .query_row(
                "SELECT state FROM subtasks WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        encoded
            .map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// Loads the durable subtask, including its current assignment and runner.
    pub fn load_subtask(&self, id: SubtaskId) -> Result<Option<Subtask>, JournalError> {
        type RawSubtask = (
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
            String,
            String,
            Option<String>,
            i64,
            Option<i64>,
            Option<i64>,
            String,
        );
        let raw: Option<RawSubtask> = self
            .connection
            .query_row(
                "SELECT dispatch_id,spec,state,node_id,attempt,base,repo_id,branch,runner_session,created_at,started_at,finished_at,usage FROM subtasks WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?,row.get(9)?,row.get(10)?,row.get(11)?,row.get(12)?)),
            )
            .optional()?;
        raw.map(
            |(
                dispatch,
                spec,
                state,
                node,
                attempt,
                base,
                repo_id,
                branch,
                runner_session,
                created,
                started,
                finished,
                usage,
            )| {
                Ok(Subtask {
                    id,
                    dispatch: DispatchId::from_str(&dispatch)
                        .map_err(|error| JournalError::IdempotencyConflict(error.to_string()))?,
                    spec: parse_json(&spec)?,
                    state: parse_json(&state)?,
                    node: node
                        .as_deref()
                        .map(|value| {
                            NodeId::from_str(value).map_err(|error| {
                                JournalError::IdempotencyConflict(error.to_string())
                            })
                        })
                        .transpose()?,
                    attempt: u16::try_from(attempt).map_err(|_| JournalError::IntegerRange)?,
                    base: GitOid(base),
                    repo_id: RepoId(GitOid(repo_id)),
                    branch,
                    runner_session,
                    created: Timestamp(created),
                    started: started.map(Timestamp),
                    finished: finished.map(Timestamp),
                    usage: parse_json(&usage)?,
                })
            },
        )
        .transpose()
    }

    /// Replaces usage accounting independently of result summaries.
    pub fn set_usage(&mut self, id: SubtaskId, usage: Usage) -> Result<(), JournalError> {
        let updated = self.connection.execute(
            "UPDATE subtasks SET usage=?2 WHERE id=?1",
            params![id.to_string(), serde_json::to_string(&usage)?],
        )?;
        if updated == 0 {
            return Err(JournalError::NotFound(format!("subtask {id}")));
        }
        Ok(())
    }

    /// Loads the separately persisted result summary for a finished subtask.
    pub fn load_result_summary(
        &self,
        id: SubtaskId,
    ) -> Result<Option<ResultSummary>, JournalError> {
        let raw: Option<ResultRow> = self
            .connection
            .query_row(
                "SELECT outcome,summary,bundle_sha256,bundle_size,branch,diffstat,assumptions,checks FROM results WHERE subtask_id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?)),
            )
            .optional()?;
        raw.map(
            |(outcome, summary, digest, size, branch, diffstat, assumptions, checks)| {
                let bundle = match (digest, size) {
                    (None, None) => None,
                    (Some(digest), Some(size)) => Some(tagteam_core::BlobRef {
                        sha256: digest.try_into().map_err(|_| {
                            JournalError::IdempotencyConflict("invalid result digest".into())
                        })?,
                        size: u64::try_from(size).map_err(|_| JournalError::IntegerRange)?,
                    }),
                    _ => {
                        return Err(JournalError::IdempotencyConflict(
                            "incomplete result blob reference".into(),
                        ));
                    }
                };
                Ok(ResultSummary {
                    outcome: serde_json::from_value(Value::String(outcome))?,
                    summary,
                    bundle,
                    branch,
                    diffstat: parse_json(&diffstat)?,
                    assumptions: parse_json(&assumptions)?,
                    checks: parse_json(&checks)?,
                })
            },
        )
        .transpose()
    }

    /// Enqueues a command idempotently by command ID and payload.
    pub fn store_command(&mut self, command: &Command) -> Result<(), JournalError> {
        let existing: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT node_id,subtask_id,op FROM commands WHERE id=?1",
                [&command.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((node, subtask, op)) = existing {
            if node == command.node_id.to_string()
                && subtask == command.subtask_id.to_string()
                && serde_json::from_str::<Value>(&op)? == command.operation
            {
                return Ok(());
            }
            return Err(JournalError::IdempotencyConflict(command.id.clone()));
        }
        self.connection.execute("INSERT INTO commands(id,node_id,subtask_id,op,state,result,sent_at,done_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![command.id,command.node_id.to_string(),command.subtask_id.to_string(),command.operation.to_string(),command.state,command.result.as_ref().map(Value::to_string),command.sent_at.map(|t|t.0),command.done_at.map(|t|t.0)])?;
        Ok(())
    }

    /// Stores the first result for a command ID; identical retries are safe.
    pub fn store_command_result(
        &mut self,
        command_id: &str,
        result: &Value,
        done_at: Timestamp,
    ) -> Result<(), JournalError> {
        let old: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT result FROM commands WHERE id=?1",
                [command_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(old) = old else {
            return Err(JournalError::NotFound(format!("command {command_id}")));
        };
        if let Some(old) = old {
            if serde_json::from_str::<Value>(&old)? == *result {
                return Ok(());
            }
            return Err(JournalError::IdempotencyConflict(command_id.into()));
        }
        self.connection.execute(
            "UPDATE commands SET result=?2,state='complete',done_at=?3 WHERE id=?1",
            params![command_id, result.to_string(), done_at.0],
        )?;
        Ok(())
    }

    /// Lists unfinished commands in stable send order for node reconnect.
    pub fn pending_commands(&self, node_id: NodeId) -> Result<Vec<Command>, JournalError> {
        let mut statement = self.connection.prepare("SELECT id,node_id,subtask_id,op,state,result,sent_at,done_at FROM commands WHERE node_id=?1 AND result IS NULL ORDER BY sent_at,id")?;
        let rows = statement.query_map([node_id.to_string()], read_command)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Loads a command and its stored result by idempotency key.
    pub fn load_command(&self, command_id: &str) -> Result<Option<Command>, JournalError> {
        self.connection
            .query_row(
                "SELECT id,node_id,subtask_id,op,state,result,sent_at,done_at FROM commands WHERE id=?1",
                [command_id],
                read_command,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Reads committed events strictly after the supplied sequence cursor.
    pub fn events(&self, filter: EventFilter<'_>) -> Result<Vec<Event>, JournalError> {
        let since = i64::try_from(filter.since_seq).map_err(|_| JournalError::IntegerRange)?;
        let mut statement = self
            .connection
            .prepare("SELECT seq,subtask_id,at,body FROM events WHERE seq>?1 ORDER BY seq")?;
        let rows = statement.query_map([since], |row| {
            let seq: i64 = row.get(0)?;
            let id: String = row.get(1)?;
            let at: i64 = row.get(2)?;
            let kind: String = row.get(3)?;
            Ok((seq, id, at, kind))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (seq, id, at, body) = row?;
            let subtask = SubtaskId::from_str(&id).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            if !matches_scope(filter.scope, subtask) {
                continue;
            }
            let kind: EventKind = serde_json::from_str(&body).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let name = event_kind_name(&kind)?;
            if !filter.kinds.is_empty() && !filter.kinds.contains(&name.as_str()) {
                continue;
            }
            events.push(Event {
                seq: u64::try_from(seq).map_err(|_| JournalError::IntegerRange)?,
                subtask,
                at: Timestamp(at),
                kind,
            });
            if filter.limit > 0 && events.len() >= filter.limit {
                break;
            }
        }
        Ok(events)
    }
}

fn store_result(
    tx: &Transaction<'_>,
    id: SubtaskId,
    result: &ResultSummary,
) -> Result<(), JournalError> {
    let outcome = serde_json::to_value(result.outcome)?;
    let outcome = outcome.as_str().ok_or_else(|| {
        JournalError::IdempotencyConflict("result outcome is not a string".into())
    })?;
    let bundle_size = result
        .bundle
        .as_ref()
        .map(|bundle| i64::try_from(bundle.size).map_err(|_| JournalError::IntegerRange))
        .transpose()?;
    tx.execute("INSERT INTO results(subtask_id,outcome,summary,bundle_sha256,bundle_size,branch,diffstat,assumptions,checks) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(subtask_id) DO UPDATE SET outcome=excluded.outcome,summary=excluded.summary,bundle_sha256=excluded.bundle_sha256,bundle_size=excluded.bundle_size,branch=excluded.branch,diffstat=excluded.diffstat,assumptions=excluded.assumptions,checks=excluded.checks",
        params![id.to_string(),outcome,result.summary,result.bundle.as_ref().map(|b|b.sha256.to_vec()),bundle_size,result.branch,serde_json::to_string(&result.diffstat)?,serde_json::to_string(&result.assumptions)?,serde_json::to_string(&result.checks)?])?;
    Ok(())
}

fn insert_event(
    tx: &Transaction<'_>,
    id: SubtaskId,
    at: Timestamp,
    kind: &EventKind,
) -> Result<u64, JournalError> {
    tx.execute(
        "INSERT INTO events(subtask_id,at,kind,body) VALUES (?1,?2,?3,?4)",
        params![
            id.to_string(),
            at.0,
            event_kind_name(kind)?,
            serde_json::to_string(kind)?
        ],
    )?;
    u64::try_from(tx.last_insert_rowid()).map_err(|_| JournalError::IntegerRange)
}

fn event_kind_name(kind: &EventKind) -> Result<String, JournalError> {
    let value = serde_json::to_value(kind)?;
    Ok(value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned())
}

fn matches_scope(scope: EventScope<'_>, id: SubtaskId) -> bool {
    match scope {
        EventScope::All => true,
        EventScope::Subtask(expected) => id == expected,
        EventScope::Watch(ids) => ids.contains(&id),
    }
}

fn read_command(row: &rusqlite::Row<'_>) -> rusqlite::Result<Command> {
    let node: String = row.get(1)?;
    let subtask: String = row.get(2)?;
    let parse_id = |value: &str, column| {
        NodeId::from_str(value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let node_id = parse_id(&node, 1)?;
    let subtask_id = SubtaskId::from_str(&subtask).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let operation: String = row.get(3)?;
    let result: Option<String> = row.get(5)?;
    Ok(Command {
        id: row.get(0)?,
        node_id,
        subtask_id,
        operation: serde_json::from_str(&operation).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        state: row.get(4)?,
        result: result
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        sent_at: row.get::<_, Option<i64>>(6)?.map(Timestamp),
        done_at: row.get::<_, Option<i64>>(7)?.map(Timestamp),
    })
}

fn parse_json<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(value)
}

fn enum_text(value: &impl Serialize) -> Result<String, JournalError> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| JournalError::IdempotencyConflict("expected a string enum value".into()))
}

#[cfg(test)]
mod tests;
