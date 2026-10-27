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
    #[cfg(unix)]
    process_group: Option<i32>,
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
        let mut command = Command::new(&start.program);
        command
            .args(&start.args)
            .current_dir(&start.cwd)
            .env(
                "TAGTEAM_RUNNER_PROTOCOL",
                RUNNER_PROTOCOL_VERSION.to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;

            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn().map_err(RunnerError::Spawn)?;
        #[cfg(unix)]
        let process_group = match child.id().and_then(|pid| i32::try_from(pid).ok()) {
            Some(pid) => pid,
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(RunnerError::ProcessGone);
            }
        };
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
            #[cfg(unix)]
            process_group: Some(process_group),
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
        #[cfg(unix)]
        {
            // Keep the child unreaped until after signaling. Its PID therefore
            // cannot be reused as an unrelated process-group ID in this window.
            tokio::time::sleep(Duration::from_secs(2)).await;
            self.kill_process_group();
            let _ = self.child.wait().await;
        }
        #[cfg(not(unix))]
        {
            if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
                .await
                .is_err()
            {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
            }
        }
    }

    /// Waits for the child to exit and returns its status.
    pub async fn wait(&mut self) -> Result<ExitStatus, RunnerError> {
        let status = self.child.wait().await;
        #[cfg(unix)]
        {
            // Once wait reaps the group leader, its numeric PID may be reused.
            self.process_group = None;
        }
        status.map_err(RunnerError::Io)
    }

    /// Kills the child and all of its descendants.
    pub async fn kill(&mut self) {
        #[cfg(unix)]
        self.kill_process_group();
        #[cfg(not(unix))]
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    #[cfg(unix)]
    fn kill_process_group(&mut self) {
        use nix::{
            sys::signal::{Signal, kill},
            unistd::Pid,
        };

        // A negative PID targets the process group created for this runner.
        // The direct child is not reaped until after this call, preventing PID
        // reuse from redirecting the signal to an unrelated group.
        if let Some(process_group) = self.process_group.take() {
            let _ = kill(Pid::from_raw(-process_group), Signal::SIGKILL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        path::Path,
        time::{Duration, Instant},
    };

    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    use tempfile::tempdir;

    use super::{RunnerProcess, RunnerStart};
    use tagteam_core::SubtaskId;

    const RUNNER_SCRIPT: &str =
        "read start_message; sleep 60 & child_pid=$!; printf '%s\\n' \"$child_pid\" > \"$1\"; wait";

    async fn start_runner(pid_file: &Path) -> (RunnerProcess, i32) {
        let process = RunnerProcess::start(RunnerStart {
            program: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                RUNNER_SCRIPT.to_owned(),
                "runner".to_owned(),
                pid_file.to_string_lossy().into_owned(),
            ],
            subtask: SubtaskId::new(),
            prompt: String::new(),
            system_append: String::new(),
            cwd: std::env::current_dir().expect("current directory is available"),
            max_turns: 1,
            wall_clock_ms: 60_000,
            tool_timeout_ms: 60_000,
            resume: None,
        })
        .await
        .expect("runner starts");
        let runner_pid = i32::try_from(process.child.id().expect("runner has a PID"))
            .expect("runner PID fits in i32");
        (process, runner_pid)
    }

    async fn read_pid_file(path: &Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = contents.trim().parse()
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "grandchild PID file was not written"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn process_exists(pid: i32) -> bool {
        match kill(Pid::from_raw(pid), None) {
            Ok(()) | Err(Errno::EPERM) => true,
            Err(Errno::ESRCH) => false,
            Err(error) => panic!("could not check process {pid}: {error}"),
        }
    }

    async fn assert_processes_exit(pids: [i32; 2]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.into_iter().any(process_exists) {
            assert!(
                Instant::now() < deadline,
                "runner process tree remained alive"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn runner_and_grandchild() -> (tempfile::TempDir, RunnerProcess, [i32; 2]) {
        let directory = tempdir().expect("temporary directory is created");
        let pid_file = directory.path().join("grandchild.pid");
        let (process, runner_pid) = start_runner(&pid_file).await;
        let grandchild_pid = read_pid_file(&pid_file).await;
        assert!(process_exists(runner_pid));
        assert!(process_exists(grandchild_pid));
        (directory, process, [runner_pid, grandchild_pid])
    }

    #[tokio::test]
    async fn kill_terminates_runner_and_grandchild() {
        let (_directory, mut process, pids) = runner_and_grandchild().await;

        process.kill().await;

        assert_processes_exit(pids).await;
    }

    #[tokio::test]
    async fn cancel_terminates_runner_and_grandchild_after_grace_period() {
        let (_directory, process, pids) = runner_and_grandchild().await;

        process.cancel("test cancellation".to_owned()).await;

        assert_processes_exit(pids).await;
    }
}
