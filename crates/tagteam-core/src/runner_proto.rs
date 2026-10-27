use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Assumption, Check, Outcome, SubtaskId, Usage};

pub const RUNNER_PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubToRunner {
    pub v: u8,
    #[serde(flatten)]
    pub message: HubToRunnerMessage,
}

impl HubToRunner {
    #[must_use]
    pub fn new(message: HubToRunnerMessage) -> Self {
        Self {
            v: RUNNER_PROTOCOL_VERSION,
            message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToRunnerMessage {
    Start {
        subtask: SubtaskId,
        prompt: String,
        system_append: String,
        cwd: PathBuf,
        max_turns: u32,
        wall_clock_ms: u64,
        tool_timeout_ms: u64,
        resume: Option<String>,
    },
    ToolResult {
        call_id: String,
        text: String,
        is_error: bool,
    },
    Cancel {
        reason: String,
    },
    Steer {
        text: String,
    },
    Answer {
        question_id: crate::QuestionId,
        call_id: String,
        text: String,
        timed_out: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerToHub {
    pub v: u8,
    #[serde(flatten)]
    pub message: RunnerToHubMessage,
}

impl RunnerToHub {
    #[must_use]
    pub fn new(message: RunnerToHubMessage) -> Self {
        Self {
            v: RUNNER_PROTOCOL_VERSION,
            message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunnerToHubMessage {
    Ready {
        session_id: String,
        claude_code_version: String,
        tools: Vec<String>,
        mcp_servers: Vec<String>,
    },
    ToolCall {
        call_id: String,
        call: RemoteCall,
    },
    Usage {
        usage: Usage,
    },
    Note {
        text: String,
    },
    RateLimited {
        detail: String,
    },
    Exited {
        subtype: String,
        error: Option<String>,
        session_id: Option<String>,
        cost_usd: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum RemoteCall {
    Bash {
        command: String,
        timeout_ms: Option<u64>,
    },
    Read {
        path: PathBuf,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    Write {
        path: PathBuf,
        content: String,
    },
    Edit {
        path: PathBuf,
        old: String,
        new: String,
        #[serde(default)]
        replace_all: bool,
    },
    Glob {
        pattern: String,
        path: Option<PathBuf>,
    },
    Grep {
        pattern: String,
        path: Option<PathBuf>,
        glob: Option<String>,
        #[serde(default)]
        ignore_case: bool,
        context: Option<u32>,
        limit: Option<u32>,
    },
    AskMain {
        question_id: crate::QuestionId,
        text: String,
        #[serde(default)]
        options: Vec<String>,
        context: Option<String>,
    },
    ReportProgress {
        note: String,
        percent: Option<u8>,
    },
    Finish {
        outcome: Outcome,
        summary: String,
        #[serde(default)]
        assumptions: Vec<Assumption>,
        #[serde(default)]
        checks: Vec<Check>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RunnerProtocolError {
    #[error("unsupported runner protocol version {0}")]
    Version(u8),
}

pub fn validate_version(version: u8) -> Result<(), RunnerProtocolError> {
    if version == RUNNER_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(RunnerProtocolError::Version(version))
    }
}

#[must_use]
pub fn wall_clock_ms(limits: &crate::Limits) -> u64 {
    u64::try_from(limits.wall_clock.as_millis()).unwrap_or(u64::MAX)
}

#[must_use]
pub fn mcp_timeout_ms(limits: &crate::Limits) -> u64 {
    let command = u64::try_from(limits.command_timeout.as_millis()).unwrap_or(u64::MAX);
    let wall = wall_clock_ms(limits);
    command
        .saturating_add(60_000)
        .min(wall.saturating_add(60_000))
}

#[must_use]
pub fn timeout_as_duration(milliseconds: u64) -> Duration {
    Duration::from_millis(milliseconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_fixture_round_trips() {
        let raw = include_str!("../../../tests/fixtures/runner-protocol/start.json");
        let message: HubToRunner = serde_json::from_str(raw).expect("parse fixture");
        validate_version(message.v).expect("version");
        assert!(matches!(message.message, HubToRunnerMessage::Start { .. }));
        let encoded = serde_json::to_string(&message).expect("encode");
        let decoded: HubToRunner = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(message, decoded);
    }

    #[test]
    fn finish_fixture_round_trips() {
        let raw = include_str!("../../../tests/fixtures/runner-protocol/finish.json");
        let message: RunnerToHub = serde_json::from_str(raw).expect("parse fixture");
        validate_version(message.v).expect("version");
        assert!(matches!(
            message.message,
            RunnerToHubMessage::ToolCall {
                call: RemoteCall::Finish { .. },
                ..
            }
        ));
        let encoded = serde_json::to_string(&message).expect("encode");
        let decoded: RunnerToHub = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(message, decoded);
    }

    #[test]
    fn mcp_timeout_covers_long_commands() {
        let limits = crate::Limits::default();
        assert_eq!(mcp_timeout_ms(&limits), 660_000);
    }
}
