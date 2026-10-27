//! Supervised Agent SDK runner process using the versioned JSON-lines protocol.
//!
//! The hub owns the child lifecycle and treats every runner field as untrusted
//! input. The runner receives only the start metadata and call results; it never
//! receives node credentials or private workspace paths.

use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use tagteam_core::{
    SubtaskId,
    runner_proto::{
        HubToRunner, HubToRunnerMessage, RUNNER_PROTOCOL_VERSION, RunnerToHub, RunnerToHubMessage,
        validate_version,
    },
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

/// Maximum JSON line accepted from a runner or written to one.
pub const MAX_RUNNER_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("could not start runner: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("runner I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("runner line exceeds the 1 MiB limit")]
    LineTooLong(#[source] tokio_util::codec::LinesCodecError),
    #[error("runner protocol is invalid: {0}")]
    Protocol(#[from] tagteam_core::runner_proto::RunnerProtocolError),
    #[error("runner emitted invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("runner closed its stdout without a final message")]
    Closed,
    #[error("runner process is unavailable")]
    ProcessGone,
}

/// A live runner child and its typed protocol streams.
pub struct RunnerProcess {
    child: Child,
    stdin: FramedWrite<ChildStdin, LinesCodec>,
    stdout: FramedRead<ChildStdout, LinesCodec>,
}

/// Immutable process and protocol settings for one runner launch.
#[derive(Debug, Clone)]
pub struct RunnerStart {
    pub program: String,
    pub args: Vec<String>,
    pub subtask: SubtaskId,
    pub prompt: String,
    pub system_append: String,
    pub cwd: PathBuf,
    pub max_turns: u32,
    pub wall_clock_ms: u64,
    pub tool_timeout_ms: u64,
    pub resume: Option<String>,
}

impl RunnerProcess {
    /// Starts a runner and sends the one startup message before returning.
    ///
    /// `cwd` is the user's repository so the SDK can load CLAUDE.md and project
    /// instructions, while all file access still goes through the remote tools.
    pub async fn start(start: RunnerStart) -> Result<Self, RunnerError> {
        if start.program.trim().is_empty() {
            return Err(RunnerError::Spawn(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "runner program must not be empty",
            )));
        }
        let mut child = Command::new(&start.program)
            .args(&start.args)
            .current_dir(&start.cwd)
            .env(
                "TAGTEAM_RUNNER_PROTOCOL",
                RUNNER_PROTOCOL_VERSION.to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(RunnerError::Spawn)?;
        let stdin = child.stdin.take().ok_or(RunnerError::ProcessGone)?;
        let stdout = child.stdout.take().ok_or(RunnerError::ProcessGone)?;
        let stderr = child.stderr.take().ok_or(RunnerError::ProcessGone)?;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "tagteam::runner", "{}", line);
            }
        });
        let mut process = Self {
            child,
            stdin: FramedWrite::new(
                stdin,
                LinesCodec::new_with_max_length(MAX_RUNNER_LINE_BYTES),
            ),
            stdout: FramedRead::new(
                stdout,
                LinesCodec::new_with_max_length(MAX_RUNNER_LINE_BYTES),
            ),
        };
        process
            .send(HubToRunnerMessage::Start {
                subtask: start.subtask,
                prompt: start.prompt,
                system_append: start.system_append,
                cwd: start.cwd,
                max_turns: start.max_turns,
                wall_clock_ms: start.wall_clock_ms,
                tool_timeout_ms: start.tool_timeout_ms,
                resume: start.resume,
            })
            .await?;
        Ok(process)
    }

    /// Sends one typed message to the runner.
    pub async fn send(&mut self, message: HubToRunnerMessage) -> Result<(), RunnerError> {
        let line = serde_json::to_string(&HubToRunner::new(message))?;
        self.stdin.send(line).await.map_err(|error| match error {
            tokio_util::codec::LinesCodecError::Io(error) => RunnerError::Io(error),
            other => RunnerError::LineTooLong(other),
        })
    }

    /// Reads the next typed runner message, validating the protocol version.
    pub async fn next(&mut self) -> Result<RunnerToHubMessage, RunnerError> {
        loop {
            let line = self
                .stdout
                .next()
                .await
                .ok_or(RunnerError::Closed)?
                .map_err(|error| match error {
                    tokio_util::codec::LinesCodecError::Io(error) => RunnerError::Io(error),
                    other => RunnerError::LineTooLong(other),
                })?;
            if line.trim().is_empty() {
                continue;
            }
            let message: RunnerToHub = serde_json::from_str(&line)?;
            validate_version(message.v)?;
            return Ok(message.message);
        }
    }

    /// Sends cancellation, then waits briefly before killing the process group.
    pub async fn cancel(mut self, reason: String) {
        let _ = self.send(HubToRunnerMessage::Cancel { reason }).await;
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }

    /// Waits for the child to exit and returns its status.
    pub async fn wait(&mut self) -> Result<ExitStatus, RunnerError> {
        self.child.wait().await.map_err(RunnerError::Io)
    }

    /// Kills the child and all of its descendants.
    pub async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}
