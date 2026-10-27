//! Durable command dispatch and confined node workspace execution.
//!
//! The supervisor persists receipts and launches every workspace operation through the
//! sandbox crate; helper subprocesses alone inspect or modify workspace contents.

use std::{
    collections::HashMap,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use tagteam_core::{Bytes, Limits};
use tagteam_sandbox::{
    CancellationHandle, CancellationToken, JobHint, ProxyEndpoint, ResourceLimits, SandboxProfile,
    spawn_confined,
};
use tokio::net::UnixListener;

use crate::{
    BeginCommand, CommandJournal, SessionSender,
    fsop::{self, FileRead, FileWrite, FsRequest},
};

use tagteam_proto::v1 as Proto;

/// Supplies already downloaded repository bundles without coupling this crate to hub blob transport.
pub trait BundleSource: Send + Sync + 'static {
    fn bundle_bytes(&self, reference: &Proto::BlobRef) -> Result<Vec<u8>, String>;
}

pub struct NoBundleSource;

impl BundleSource for NoBundleSource {
    fn bundle_bytes(&self, _reference: &Proto::BlobRef) -> Result<Vec<u8>, String> {
        Err("bundle source is not configured".into())
    }
}

#[derive(Clone)]
pub struct CommandRunner {
    data_dir: PathBuf,
    executable: Option<PathBuf>,
    journal: CommandJournal,
    bundles: Arc<dyn BundleSource>,
    running: Arc<Mutex<HashMap<String, CancellationHandle>>>,
    workspace_limits: Arc<Mutex<HashMap<String, WorkspaceLimits>>>,
    execution_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone)]
struct WorkspaceLimits {
    resources: ResourceLimits,
    jobs: u32,
}

impl CommandRunner {
    pub fn new(
        data_dir: impl Into<PathBuf>,
        journal: CommandJournal,
        bundles: impl BundleSource,
    ) -> Self {
        Self {
            data_dir: data_dir.into(),
            executable: std::env::current_exe().ok(),
            journal,
            bundles: Arc::new(bundles),
            running: Arc::default(),
            workspace_limits: Arc::default(),
            execution_lock: Arc::default(),
        }
    }

    /// Overrides the helper executable, primarily to exercise the built CLI integration path.
    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    pub async fn execute_command(
        &self,
        command: &Proto::Command,
        token: CancellationToken,
    ) -> Result<Proto::CommandResult, String> {
        self.execute(command, token).await
    }

    /// Records a receipt synchronously, then runs once in a task so cancellation events remain live.
    pub fn dispatch(&self, command: Proto::Command, sender: SessionSender) {
        if command.command_id.is_empty() {
            tokio::spawn(async move {
                let _ = sender
                    .send_result(error_result("", "command ID is empty"))
                    .await;
            });
            return;
        }
        match self.journal.begin(&command.command_id) {
            Ok(BeginCommand::Completed(ack)) => {
                tokio::spawn(async move {
                    let _ = sender.send_ack(ack).await;
                });
            }
            Ok(BeginCommand::InProgress) => {}
            Err(_) => {
                tokio::spawn(async move {
                    let _ = sender
                        .send_result(error_result(
                            &command.command_id,
                            "command receipt could not be persisted",
                        ))
                        .await;
                });
            }
            Ok(BeginCommand::Execute) => {
                let runner = self.clone();
                let id = command.command_id.clone();
                let (handle, token) = CancellationToken::new();
                if let Ok(mut running) = self.running.lock() {
                    running.insert(id.clone(), handle);
                }
                tokio::spawn(async move {
                    let _execution = runner.execution_lock.lock().await;
                    let started = Instant::now();
                    let mut result = runner
                        .execute(&command, token)
                        .await
                        .unwrap_or_else(|error| error_result(&id, &error));
                    if let Some(Proto::command_result::Result::Exec(ref mut value)) = result.result
                    {
                        value.took_ms =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    }
                    let ack = Proto::CommandAck {
                        command_id: id.clone(),
                        result: Some(result.clone()),
                    };
                    if runner.journal.complete(&ack).is_ok() {
                        if let Some(Proto::command_result::Result::Exec(exec)) = &result.result {
                            if !exec.stdout_tail.is_empty() {
                                let _ = sender
                                    .send_output(Proto::OutputChunk {
                                        command_id: id.clone(),
                                        stream: Proto::StreamKind::Stdout as i32,
                                        data: exec.stdout_tail.clone(),
                                    })
                                    .await;
                            }
                            if !exec.stderr_tail.is_empty() {
                                let _ = sender
                                    .send_output(Proto::OutputChunk {
                                        command_id: id.clone(),
                                        stream: Proto::StreamKind::Stderr as i32,
                                        data: exec.stderr_tail.clone(),
                                    })
                                    .await;
                            }
                        }
                        let _ = sender.send_result(result).await;
                    }
                    if let Ok(mut running) = runner.running.lock() {
                        running.remove(&id);
                    }
                });
            }
        }
    }

    pub fn cancel(&self, command_id: &str) {
        if let Ok(running) = self.running.lock()
            && let Some(handle) = running.get(command_id)
        {
            handle.cancel();
        }
    }

    async fn execute(
        &self,
        command: &Proto::Command,
        token: CancellationToken,
    ) -> Result<Proto::CommandResult, String> {
        let op = command.op.as_ref().ok_or("command operation is missing")?;
        match op {
            Proto::command::Op::Prepare(prepare) => {
                let reference = prepare
                    .bundle
                    .as_ref()
                    .ok_or("bundle reference is missing")?;
                let bytes = self.bundles.bundle_bytes(reference)?;
                let workspace = self.prepare_workspace(&command.workspace)?;
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Prepare {
                            bundle: STANDARD.encode(bytes),
                            base: prepare.base.clone(),
                            branch: prepare.branch.clone(),
                        },
                        token,
                        Limits::default(),
                    )
                    .await?;
                check_response(response)?;
                if let Some(profile) = &prepare.profile {
                    let resources = ResourceLimits::new(
                        Bytes(profile.memory),
                        std::num::NonZeroU32::new(profile.pids.max(1))
                            .ok_or("invalid process limit")?,
                        Bytes(profile.disk),
                        std::time::Duration::from_millis(100),
                    )
                    .map_err(|_| "sandbox limits are invalid")?;
                    let jobs = if profile.cpu_cores.is_finite() && profile.cpu_cores > 0.0 {
                        profile.cpu_cores.ceil().clamp(1.0, u32::MAX as f32) as u32
                    } else {
                        1
                    };
                    if let Ok(mut configured) = self.workspace_limits.lock() {
                        configured.insert(
                            command.workspace.clone(),
                            WorkspaceLimits { resources, jobs },
                        );
                    }
                }
                Ok(done(&command.command_id, true))
            }
            Proto::command::Op::Exec(exec) => {
                let workspace = self.workspace(&command.workspace)?;
                self.ensure_workspace(&workspace)?;
                let request = fsop::ExecRequest {
                    argv: exec.argv.clone(),
                    cwd: exec.cwd.clone(),
                    env: exec.env.clone().into_iter().collect(),
                    stdin: STANDARD.encode(&exec.stdin),
                };
                let executable = self
                    .executable
                    .as_ref()
                    .ok_or("node executable unavailable")?;
                let encoded = STANDARD.encode(
                    serde_json::to_vec(&request).map_err(|_| "command could not be encoded")?,
                );
                let command_line = format!(
                    "exec {} __exec {}",
                    shell_quote(
                        executable
                            .to_str()
                            .ok_or("node executable path is invalid")?
                    ),
                    shell_quote(&encoded)
                );
                let limits = limits_for(exec.timeout_ms, exec.output_cap);
                let (outcome, _listener) = self
                    .spawn(&workspace, command_line, token, limits, Vec::new())
                    .await?;
                let mut result = Proto::ExecDone {
                    exit_code: outcome.exit_code.unwrap_or(-1),
                    signal: outcome.signal.unwrap_or(0),
                    timed_out: outcome.timed_out,
                    killed: outcome.cancelled || outcome.resource_limit.is_some(),
                    stdout_tail: outcome.stdout.clone(),
                    stderr_tail: outcome.stderr.clone(),
                    truncated: outcome.truncated,
                    took_ms: 0,
                    peak_memory: 0,
                };
                if outcome.exit_code.is_none() && outcome.signal.is_none() {
                    result.exit_code = -1;
                }
                Ok(Proto::CommandResult {
                    command_id: command.command_id.clone(),
                    result: Some(Proto::command_result::Result::Exec(result)),
                })
            }
            Proto::command::Op::Read(read) => {
                let workspace = self.workspace(&command.workspace)?;
                let files = read
                    .files
                    .iter()
                    .map(|file| FileRead {
                        path: file.path.clone(),
                        offset: file.offset,
                        limit: file.limit,
                    })
                    .collect();
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Read { files },
                        token,
                        Limits::default(),
                    )
                    .await?;
                let values = response
                    .get("files")
                    .and_then(Value::as_array)
                    .ok_or("read helper returned invalid data")?;
                let files = values
                    .iter()
                    .map(|value| {
                        let data = value
                            .get("data")
                            .and_then(Value::as_str)
                            .ok_or("read helper returned invalid data")?;
                        Ok(Proto::FileBody {
                            path: value
                                .get("path")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            data: STANDARD
                                .decode(data)
                                .map_err(|_| "read helper returned invalid data")?,
                            expected_sha256: None,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(Proto::CommandResult {
                    command_id: command.command_id.clone(),
                    result: Some(Proto::command_result::Result::Read(Proto::FilesRead {
                        files,
                    })),
                })
            }
            Proto::command::Op::Write(write) => {
                let workspace = self.workspace(&command.workspace)?;
                let files = write
                    .files
                    .iter()
                    .map(|file| FileWrite {
                        path: file.path.clone(),
                        data: STANDARD.encode(&file.data),
                        expected_sha256: file
                            .expected_sha256
                            .as_ref()
                            .map(|bytes| data_encoding::HEXLOWER.encode(bytes)),
                    })
                    .collect();
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Write { files },
                        token,
                        Limits::default(),
                    )
                    .await?;
                let paths = response
                    .get("paths")
                    .and_then(Value::as_array)
                    .ok_or("write helper returned invalid data")?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                Ok(Proto::CommandResult {
                    command_id: command.command_id.clone(),
                    result: Some(Proto::command_result::Result::Written(Proto::Written {
                        paths,
                    })),
                })
            }
            Proto::command::Op::Edit(edit) => {
                let workspace = self.workspace(&command.workspace)?;
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Edit {
                            path: edit.path.clone(),
                            old: edit.old.clone(),
                            new: edit.new.clone(),
                            replace_all: edit.replace_all,
                        },
                        token,
                        Limits::default(),
                    )
                    .await?;
                Ok(Proto::CommandResult {
                    command_id: command.command_id.clone(),
                    result: Some(Proto::command_result::Result::Edited(Proto::Edited {
                        path: edit.path.clone(),
                        replacements: json_u32(&response, "replacements")?,
                    })),
                })
            }
            Proto::command::Op::Glob(glob) => {
                let workspace = self.workspace(&command.workspace)?;
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Glob {
                            pattern: glob.pattern.clone(),
                            cwd: glob.cwd.clone(),
                            limit: glob.limit,
                        },
                        token,
                        Limits::default(),
                    )
                    .await?;
                Ok(matches_result(&command.command_id, response))
            }
            Proto::command::Op::Grep(grep) => {
                let workspace = self.workspace(&command.workspace)?;
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Grep {
                            pattern: grep.pattern.clone(),
                            path: grep.path.clone(),
                            glob: grep.glob.clone(),
                            ignore_case: grep.ignore_case,
                            context: grep.context,
                            limit: grep.limit,
                        },
                        token,
                        Limits::default(),
                    )
                    .await?;
                Ok(matches_result(&command.command_id, response))
            }
            Proto::command::Op::Commit(commit) => {
                let workspace = self.workspace(&command.workspace)?;
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Commit {
                            message: commit.message.clone(),
                        },
                        token,
                        Limits::default(),
                    )
                    .await?;
                let oid = response
                    .get("oid")
                    .and_then(Value::as_str)
                    .ok_or("git helper returned invalid commit")?
                    .to_owned();
                Ok(Proto::CommandResult {
                    command_id: command.command_id.clone(),
                    result: Some(Proto::command_result::Result::Committed(Proto::Committed {
                        oid,
                    })),
                })
            }
            Proto::command::Op::Collect(collect) => {
                let workspace = self.workspace(&command.workspace)?;
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Collect {
                            base: collect.base.clone(),
                        },
                        token,
                        Limits::default(),
                    )
                    .await?;
                let encoded = response
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or("git helper returned invalid bundle")?;
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| "git helper returned invalid bundle")?;
                let digest = sha2::Sha256::digest(&bytes);
                self.store_blob(&digest, &bytes)?;
                Ok(Proto::CommandResult {
                    command_id: command.command_id.clone(),
                    result: Some(Proto::command_result::Result::Collected(Proto::BlobRef {
                        sha256: digest.to_vec(),
                        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    })),
                })
            }
            Proto::command::Op::Cleanup(cleanup) => {
                let workspace = self.workspace(&command.workspace)?;
                if fs::symlink_metadata(&workspace).is_ok() {
                    let data_dir = self
                        .data_dir
                        .canonicalize()
                        .map_err(|_| "node data directory is unavailable")?;
                    if workspace
                        .symlink_metadata()
                        .map_err(|_| "workspace is unavailable")?
                        .file_type()
                        .is_symlink()
                        || !workspace
                            .canonicalize()
                            .map_err(|_| "workspace is unavailable")?
                            .starts_with(data_dir)
                    {
                        return Err("workspace path escapes node data directory".into());
                    }
                    fs::remove_dir_all(workspace).map_err(|_| "workspace cleanup failed")?;
                }
                let _ = cleanup.keep_cache;
                Ok(done(&command.command_id, true))
            }
        }
    }

    fn workspace(&self, id: &str) -> Result<PathBuf, String> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err("workspace ID is invalid".into());
        }
        Ok(self.data_dir.join("workspaces").join(id))
    }

    fn ensure_workspace(&self, path: &Path) -> Result<(), String> {
        let data_dir = self
            .data_dir
            .canonicalize()
            .map_err(|_| "node data directory is unavailable")?;
        let workspaces = self.data_dir.join("workspaces");
        ensure_private_directory(&workspaces)?;
        ensure_private_directory(path)?;
        let canonical_workspace = path
            .canonicalize()
            .map_err(|_| "workspace could not be prepared")?;
        if !canonical_workspace.starts_with(&data_dir) {
            return Err("workspace path escapes node data directory".into());
        }
        for name in ["repo", "home", "tmp"] {
            ensure_private_directory(&path.join(name))?;
        }
        Ok(())
    }

    fn prepare_workspace(&self, id: &str) -> Result<PathBuf, String> {
        let workspace = self.workspace(id)?;
        self.ensure_workspace(&workspace)?;
        Ok(workspace)
    }

    fn store_blob(&self, digest: &[u8], bytes: &[u8]) -> Result<(), String> {
        let directory = self.data_dir.join("blob-cache");
        fs::create_dir_all(&directory).map_err(|_| "blob cache is unavailable")?;
        let path = directory.join(data_encoding::HEXLOWER.encode(digest));
        if !path.exists() {
            let temp = directory.join(format!(".blob-{}.tmp", std::process::id()));
            use std::io::Write;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temp)
                .map_err(|_| "blob cache is unavailable")?;
            file.write_all(bytes)
                .map_err(|_| "blob cache is unavailable")?;
            file.sync_all().map_err(|_| "blob cache is unavailable")?;
            fs::rename(temp, path).map_err(|_| "blob cache is unavailable")?;
        }
        Ok(())
    }

    async fn run_fsop(
        &self,
        workspace: &Path,
        request: FsRequest,
        token: CancellationToken,
        limits: Limits,
    ) -> Result<Value, String> {
        self.ensure_workspace(workspace)?;
        let encoded = fsop::encode_request(&request)?;
        let executable = self
            .executable
            .as_ref()
            .ok_or("node executable unavailable")?;
        let command = format!(
            "exec {} __fsop {}",
            shell_quote(&executable.display().to_string()),
            shell_quote(&encoded)
        );
        let (outcome, _listener) = self
            .spawn(workspace, command, token, limits, Vec::new())
            .await?;
        if outcome.exit_code != Some(0) {
            return Err("sandboxed workspace operation failed".into());
        }
        let line = std::str::from_utf8(&outcome.stdout)
            .map_err(|_| "workspace helper returned invalid output")?
            .trim();
        let bytes = STANDARD
            .decode(line)
            .map_err(|_| "workspace helper returned invalid output")?;
        let envelope: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "workspace helper returned invalid output")?;
        if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(envelope
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("workspace operation failed")
                .to_owned());
        }
        envelope
            .get("value")
            .cloned()
            .ok_or_else(|| "workspace helper returned invalid output".into())
    }

    async fn spawn(
        &self,
        workspace: &Path,
        command: String,
        token: CancellationToken,
        limits: Limits,
        environment: Vec<(String, String)>,
    ) -> Result<(tagteam_sandbox::SandboxOutcome, UnixListener), String> {
        self.ensure_workspace(workspace)?;
        let executable = self
            .executable
            .as_ref()
            .ok_or("node executable unavailable")?;
        let socket = workspace.join("tmp/proxy.sock");
        if fs::symlink_metadata(&socket).is_ok() {
            fs::remove_file(&socket).map_err(|_| "sandbox proxy socket is unavailable")?;
        }
        let listener =
            UnixListener::bind(&socket).map_err(|_| "sandbox proxy socket is unavailable")?;
        let proxy = ProxyEndpoint::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1), socket)
            .map_err(|_| "sandbox proxy profile is invalid")?;
        let id = workspace
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let configured = self
            .workspace_limits
            .lock()
            .ok()
            .and_then(|values| values.get(id).cloned());
        let (resource_limits, jobs) = if let Some(value) = configured {
            (value.resources, value.jobs)
        } else {
            (
                ResourceLimits::new(
                    Bytes::gib(2),
                    std::num::NonZeroU32::new(64).ok_or("invalid process limit")?,
                    Bytes::gib(4),
                    std::time::Duration::from_millis(100),
                )
                .map_err(|_| "sandbox limits are invalid")?,
                4,
            )
        };
        let mut profile = SandboxProfile::new(workspace, command, proxy, limits, resource_limits)
            .map_err(|_| "sandbox profile is invalid")?;
        profile = profile.with_job_hint(JobHint::new(
            std::num::NonZeroU32::new(jobs).ok_or("invalid CPU limit")?,
        ));
        let executable_dir = executable
            .parent()
            .ok_or("node executable unavailable")?
            .to_path_buf();
        profile = profile
            .with_toolchain_root(executable_dir)
            .map_err(|_| "sandbox toolchain root is invalid")?;
        for (key, value) in environment {
            profile = profile
                .with_environment(key, value)
                .map_err(|_| "command environment is invalid")?;
        }
        let result = spawn_confined(profile, token)
            .await
            .map_err(|_| "confined command could not run")?;
        Ok((result, listener))
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err("workspace path is unsafe".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| "workspace could not be prepared")?;
        }
        Err(_) => return Err("workspace path is unsafe".into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "workspace permissions could not be set")?;
    }
    Ok(())
}

impl crate::PendingResultSource for CommandJournal {
    fn pending_results(&self) -> Result<Vec<Proto::CommandAck>, crate::SessionError> {
        self.pending()
            .map_err(|_| crate::SessionError::PendingResults)
    }
}

fn limits_for(timeout_ms: u32, output_cap: u32) -> Limits {
    let mut limits = Limits::default();
    if timeout_ms > 0 {
        limits.command_timeout = std::time::Duration::from_millis(u64::from(timeout_ms));
    }
    if output_cap > 0 {
        limits.max_output = Bytes(u64::from(output_cap));
    }
    limits
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn done(command_id: &str, ok: bool) -> Proto::CommandResult {
    Proto::CommandResult {
        command_id: command_id.to_owned(),
        result: Some(Proto::command_result::Result::Done(Proto::Done { ok })),
    }
}

fn error_result(command_id: &str, message: &str) -> Proto::CommandResult {
    Proto::CommandResult {
        command_id: command_id.to_owned(),
        result: Some(Proto::command_result::Result::Error(Proto::Error {
            code: Proto::ErrorCode::Internal as i32,
            message: message.to_owned(),
            retryable: false,
        })),
    }
}

fn check_response(value: Value) -> Result<(), String> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err("workspace operation failed".into())
    }
}

fn json_u32(value: &Value, field: &str) -> Result<u32, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| "workspace helper returned invalid data".into())
}

fn matches_result(command_id: &str, value: Value) -> Proto::CommandResult {
    let lines = value
        .get("lines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let truncated = value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Proto::CommandResult {
        command_id: command_id.to_owned(),
        result: Some(Proto::command_result::Result::Matches(Proto::Matches {
            lines,
            truncated,
        })),
    }
}

#[allow(dead_code)]
fn _assert_request_serializable<T: Serialize>(_value: &T) {}
