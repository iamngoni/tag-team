use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    AnsweredBy, DispatchId, Event, MainSessionId, Node, QuestionId, ResultSummary, SubtaskId,
    SubtaskSpec, Usage, schedule::Placement,
};

/// Version carried by the private CLI/MCP attach handshake.
pub const LOCAL_PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "id", rename_all = "snake_case")]
pub enum WatchScope {
    All,
    Dispatch(DispatchId),
    Subtask(SubtaskId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchInput {
    pub main_session: MainSessionId,
    pub claude_session: Option<String>,
    pub repo: PathBuf,
    pub subtasks: Vec<SubtaskSpec>,
    #[serde(default)]
    pub spread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReply {
    pub dispatch: DispatchId,
    pub subtasks: Vec<(SubtaskId, Placement)>,
    /// Working-tree files omitted from the snapshot by the secret denylist.
    pub excluded_paths: Vec<String>,
    pub watch_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReply {
    pub subtasks: Vec<SubtaskStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskStatus {
    pub id: SubtaskId,
    pub title: String,
    pub state: String,
    pub node: Option<String>,
    pub usage: Usage,
    pub last_progress: Option<String>,
    pub open_question: Option<String>,
    pub open_question_text: Option<String>,
    pub question_deadline: Option<crate::Timestamp>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsReply {
    pub subtask: SubtaskId,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectReply {
    pub subtask: SubtaskId,
    pub result: ResultSummary,
    pub worktree: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefReply {
    pub running: u32,
    pub done: u32,
    pub waiting: u32,
}

impl BriefReply {
    #[must_use]
    pub fn display(&self) -> String {
        format!(
            "tag-team: {} running, {} done, {} waiting",
            self.running, self.done, self.waiting
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    pub node: Node,
    pub last_seen_age: Duration,
    pub running_subtasks: Vec<SubtaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteReply {
    pub invite: String,
    pub name: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerAuthor {
    MainClaude,
    You,
}

impl AnswerAuthor {
    #[must_use]
    pub const fn answered_by(self) -> AnsweredBy {
        match self {
            Self::MainClaude => AnsweredBy::MainClaude,
            Self::You => AnsweredBy::You,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerReply {
    pub subtask: SubtaskId,
    pub question: QuestionId,
    pub text: String,
    pub by: AnsweredBy,
    pub delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalRequest {
    Ping,
    Attach {
        version: u8,
        pid: u32,
        cwd: PathBuf,
    },
    BindClaudeSession {
        main_session: MainSessionId,
        claude_session: String,
    },
    Nodes,
    Dispatch(DispatchInput),
    Status {
        ids: Option<Vec<SubtaskId>>,
        #[serde(default)]
        claude_session: Option<String>,
    },
    Inbox {
        since_seq: u64,
        wait_ms: u64,
        kinds: Option<Vec<String>>,
        all: bool,
        #[serde(default)]
        claude_session: Option<String>,
    },
    Watch {
        scope: WatchScope,
        since_seq: u64,
    },
    Logs {
        subtask: SubtaskId,
        tail: u32,
        #[serde(default)]
        claude_session: Option<String>,
    },
    Cancel {
        subtask: SubtaskId,
        reason: String,
        #[serde(default)]
        claude_session: Option<String>,
    },
    Collect {
        subtask: SubtaskId,
        worktree: bool,
        #[serde(default)]
        claude_session: Option<String>,
    },
    Answer {
        subtask: SubtaskId,
        #[serde(default)]
        question: Option<QuestionId>,
        text: String,
        author: AnswerAuthor,
        #[serde(default)]
        claude_session: Option<String>,
    },
    Steer {
        subtask: SubtaskId,
        text: String,
        #[serde(default)]
        claude_session: Option<String>,
    },
    Brief {
        claude_session: String,
    },
    Invite {
        name: String,
        ttl_seconds: u64,
    },
    Approve {
        name: String,
    },
    Pause {
        name: String,
    },
    Resume {
        name: String,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum LocalResponseBody {
    Pong,
    Attached { session: MainSessionId },
    Bound,
    Nodes(Vec<NodeView>),
    Dispatched(DispatchReply),
    Status(StatusReply),
    Events(Vec<Event>),
    Logs(LogsReply),
    Cancelled { subtask: SubtaskId },
    Collected(CollectReply),
    Answered(AnswerReply),
    Steered { subtask: SubtaskId },
    Brief(BriefReply),
    Invited(InviteReply),
    NodeChanged { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocalFrame {
    Request {
        id: u64,
        request: LocalRequest,
    },
    Response {
        id: u64,
        response: LocalResponseBody,
    },
    Event {
        id: u64,
        event: Event,
    },
    Error {
        id: u64,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_text_is_stable() {
        assert_eq!(
            BriefReply {
                running: 2,
                done: 1,
                waiting: 0
            }
            .display(),
            "tag-team: 2 running, 1 done, 0 waiting"
        );
    }

    #[test]
    fn local_frames_round_trip() {
        let frame = LocalFrame::Request {
            id: 7,
            request: LocalRequest::Ping,
        };
        let encoded = serde_json::to_string(&frame).expect("encode");
        let decoded: LocalFrame = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(frame, decoded);
    }
}
