//! Durable command dispatch and confined node workspace execution.
//!
//! The supervisor persists receipts and launches every workspace operation through the
//! sandbox crate; helper subprocesses alone inspect or modify workspace contents.

#[cfg(target_os = "linux")]
use std::net::{IpAddr, Ipv4Addr};
use std::{
    collections::{HashMap, HashSet},
    fs,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::Value;
use tagteam_core::{Bytes, Limits, MAX_BLOB_BYTES, NodePolicy};
#[cfg(target_os = "macos")]
use tagteam_proxy::ProxyServer;
#[cfg(target_os = "linux")]
use tagteam_proxy::UnixProxyServer;
use tagteam_proxy::{AllowedHost, DEFAULT_EGRESS, ProxyConfig, TcpConnector, TokioResolver};
use tagteam_sandbox::{
    CancellationHandle, CancellationToken, JobHint, ProxyEndpoint, ResourceLimits, SandboxProfile,
    spawn_confined,
};

use crate::{
    BeginCommand, CommandJournal, SessionSender,
    fsop::{self, FileRead, FileWrite, FsRequest},
};

use tagteam_proto::v1 as Proto;

/// Supplies already downloaded repository bundles without coupling this crate to hub blob transport.
pub trait BundleSource: Send + Sync + 'static {
    fn bundle_bytes(
        &self,
        reference: &Proto::BlobRef,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + '_>>;

    /// Stores a bounded result bundle and returns its verified hub reference.
    fn store_bundle(
        &self,
        _bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Proto::BlobRef, String>> + Send + '_>> {
        Box::pin(async { Err("bundle source does not support uploads".into()) })
    }
}

pub struct NoBundleSource;

impl BundleSource for NoBundleSource {
    fn bundle_bytes(
        &self,
        _reference: &Proto::BlobRef,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + '_>> {
        Box::pin(async { Err("bundle source is not configured".into()) })
    }
}

#[derive(Clone)]
pub struct CommandRunner {
    data_dir: PathBuf,
    executable: Option<PathBuf>,
    journal: CommandJournal,
    bundles: Arc<dyn BundleSource>,
    git_toolchain: Option<GitToolchain>,
    running: Arc<Mutex<HashMap<String, CommandCancellation>>>,
    workspace_limits: Arc<Mutex<HashMap<String, WorkspaceLimits>>>,
    workspace_egress: Arc<Mutex<HashMap<String, Vec<AllowedHost>>>>,
    policy: NodePolicy,
    allowed_repo_ids: Option<Vec<String>>,
    execution_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Tracks whether a command has a live cancellation token or was cancelled
/// before its dispatch task could register one. Pending entries deliberately do
/// not expire: a delayed replay of the command must still be suppressed.
enum CommandCancellation {
    Pending,
    Active(CancellationHandle),
}

impl CommandCancellation {
    fn request(&self) {
        if let Self::Active(handle) = self {
            handle.cancel();
        }
    }
}

#[derive(Clone)]
struct WorkspaceLimits {
    resources: ResourceLimits,
    jobs: u32,
}

const MAX_EGRESS_HOSTS: usize = 256;
#[cfg(target_os = "linux")]
const LINUX_PROXY_BRIDGE_PORT: u16 = 38_417;

enum RunningProxy {
    #[cfg(target_os = "macos")]
    Tcp(ProxyServer),
    #[cfg(target_os = "linux")]
    Unix(UnixProxyServer),
}

impl RunningProxy {
    async fn shutdown(self) {
        match self {
            #[cfg(target_os = "macos")]
            Self::Tcp(proxy) => proxy.shutdown().await,
            #[cfg(target_os = "linux")]
            Self::Unix(proxy) => proxy.shutdown().await,
        }
    }
}

/// The active macOS developer toolchain used to avoid the Xcode command shim.
///
/// `/usr/bin/git` shells out through `xcode-select`, which reads host-only
/// paths that the workspace sandbox deliberately denies. Resolving the active
/// developer directory once lets the confined helper run its real Git binary
/// from a read-only toolchain root instead.
#[derive(Clone)]
struct GitToolchain {
    developer_dir: PathBuf,
    executable: PathBuf,
}

fn host_git_toolchain() -> Option<GitToolchain> {
    #[cfg(target_os = "macos")]
    {
        let developer_dir = fs::read_link("/var/select/developer_dir").ok()?;
        let executable = developer_dir.join("usr/bin/git");
        executable.is_file().then_some(GitToolchain {
            developer_dir,
            executable,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
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
            git_toolchain: host_git_toolchain(),
            running: Arc::default(),
            workspace_limits: Arc::default(),
            workspace_egress: Arc::default(),
            policy: NodePolicy::default(),
            allowed_repo_ids: None,
            execution_lock: Arc::default(),
        }
    }

    /// Applies the owner's local policy to every command. The hub policy is
    /// advisory for scheduling; these checks are the node's enforcement point.
    pub fn with_policy(
        mut self,
        policy: NodePolicy,
        allowed_repo_ids: Option<Vec<String>>,
    ) -> Self {
        self.policy = policy;
        self.allowed_repo_ids = allowed_repo_ids.map(|ids| {
            ids.into_iter()
                .map(|id| id.to_ascii_lowercase())
                .collect::<Vec<_>>()
        });
        self
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
                if let Ok(mut running) = self.running.lock() {
                    running.remove(&command.command_id);
                }
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
                let registration = match self.running.lock() {
                    Ok(mut running) => match running.entry(id.clone()) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(CommandCancellation::Active(handle));
                            Ok(())
                        }
                        std::collections::hash_map::Entry::Occupied(entry)
                            if matches!(entry.get(), CommandCancellation::Pending) =>
                        {
                            entry.remove();
                            Err(true)
                        }
                        std::collections::hash_map::Entry::Occupied(_) => Err(false),
                    },
                    Err(_) => Err(false),
                };
                match registration {
                    Ok(()) => {}
                    Err(cancelled_before_dispatch) => {
                        let message = if cancelled_before_dispatch {
                            "command was cancelled before dispatch"
                        } else {
                            "command cancellation state is unavailable"
                        };
                        let result = if cancelled_before_dispatch {
                            cancelled_result(&id)
                        } else {
                            error_result(&id, message)
                        };
                        tokio::spawn(async move {
                            let ack = Proto::CommandAck {
                                command_id: id.clone(),
                                result: Some(result.clone()),
                            };
                            if runner.journal.complete(&ack).is_ok() {
                                let _ = sender.send_result(result).await;
                            }
                        });
                        return;
                    }
                }
                tokio::spawn(async move {
                    let _execution = runner.execution_lock.lock().await;
                    let started = Instant::now();
                    // A cancellation that wins the registration race after the
                    // journal receipt was written still prevents this task from
                    // starting any workspace operation.
                    let mut result = if token.is_cancelled() {
                        cancelled_result(&id)
                    } else {
                        runner
                            .execute(&command, token)
                            .await
                            .unwrap_or_else(|error| error_result(&id, &error))
                    };
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

    /// Requests cancellation and records it durably before a command can be
    /// dispatched, including across a node restart.
    pub fn cancel(&self, command_id: &str) {
        if command_id.is_empty() {
            return;
        }
        if let Ok(mut running) = self.running.lock() {
            // Keep the in-memory registration lock while persisting the marker.
            // Dispatch takes the same lock after its receipt lookup, so it sees
            // either Pending or the durable Completed acknowledgement.
            if let Err(error) = self.journal.cancel(command_id) {
                tracing::error!(%error, "command cancellation could not be persisted");
            }
            match running.entry(command_id.to_owned()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.get().request(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(CommandCancellation::Pending);
                }
            }
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
                self.check_prepare_policy(prepare)?;
                let egress_hosts = self.allowed_egress_hosts(prepare)?;
                let reference = prepare
                    .bundle
                    .as_ref()
                    .ok_or("bundle reference is missing")?;
                let bytes = self.bundles.bundle_bytes(reference).await?;
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
                self.workspace_egress
                    .lock()
                    .map_err(|_| "workspace egress policy is unavailable")?
                    .insert(command.workspace.clone(), egress_hosts);
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
                    "exec {} __exec",
                    shell_quote(
                        executable
                            .to_str()
                            .ok_or("node executable path is invalid")?
                    )
                );
                let limits = limits_for(exec.timeout_ms, exec.output_cap);
                let outcome = self
                    .spawn(
                        &workspace,
                        command_line,
                        token,
                        limits,
                        Vec::new(),
                        encoded.into_bytes(),
                    )
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
                let message = collect.message.chars().take(512).collect::<String>();
                let response = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Collect {
                            base: collect.base.clone(),
                            message,
                        },
                        token.clone(),
                        Limits::default(),
                    )
                    .await?;
                let path = response
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("git helper returned an invalid bundle path")?;
                let size = response
                    .get("size")
                    .and_then(Value::as_u64)
                    .ok_or("git helper returned an invalid bundle size")?;
                if size > tagteam_core::MAX_BLOB_BYTES {
                    let _ = self
                        .run_fsop(
                            &workspace,
                            FsRequest::Delete {
                                path: path.to_owned(),
                            },
                            token.clone(),
                            Limits::default(),
                        )
                        .await;
                    return Err("result bundle exceeds the allowed size".into());
                }
                const READ_CHUNK: u64 = 128 * 1024;
                let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
                let mut offset = 0;
                while offset < size {
                    let limit = READ_CHUNK.min(size - offset);
                    let response = self
                        .run_fsop(
                            &workspace,
                            FsRequest::Read {
                                files: vec![FileRead {
                                    path: path.to_owned(),
                                    offset,
                                    limit,
                                }],
                            },
                            token.clone(),
                            Limits::default(),
                        )
                        .await?;
                    let encoded = response
                        .get("files")
                        .and_then(Value::as_array)
                        .and_then(|files| files.first())
                        .and_then(|file| file.get("data"))
                        .and_then(Value::as_str)
                        .ok_or("git helper returned invalid bundle data")?;
                    let chunk = STANDARD
                        .decode(encoded)
                        .map_err(|_| "git helper returned invalid bundle data")?;
                    if chunk.is_empty() {
                        return Err("result bundle read was empty".into());
                    }
                    let next = offset
                        .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
                        .ok_or("result bundle read exceeded its size")?;
                    if next > size
                        || u64::try_from(bytes.len().saturating_add(chunk.len()))
                            .map_or(true, |total| total > MAX_BLOB_BYTES)
                    {
                        return Err("result bundle read exceeded its declared size".into());
                    }
                    bytes.extend_from_slice(&chunk);
                    offset = next;
                }
                let _ = self
                    .run_fsop(
                        &workspace,
                        FsRequest::Delete {
                            path: path.to_owned(),
                        },
                        token.clone(),
                        Limits::default(),
                    )
                    .await;
                let reference = self.bundles.store_bundle(bytes).await?;
                Ok(Proto::CommandResult {
                    command_id: command.command_id.clone(),
                    result: Some(Proto::command_result::Result::Collected(reference)),
                })
            }
            Proto::command::Op::Cleanup(cleanup) => {
                let workspace = self.workspace(&command.workspace)?;
                self.workspace_egress
                    .lock()
                    .map_err(|_| "workspace egress policy is unavailable")?
                    .remove(&command.workspace);
                self.workspace_limits
                    .lock()
                    .map_err(|_| "workspace limits are unavailable")?
                    .remove(&command.workspace);
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

    fn check_prepare_policy(&self, prepare: &Proto::PrepareWorkspace) -> Result<(), String> {
        self.allowed_egress_hosts(prepare)?;
        if let Some(allowed) = &self.allowed_repo_ids {
            let repo = prepare.repo_id.to_ascii_lowercase();
            if repo.is_empty() || !allowed.iter().any(|candidate| candidate == &repo) {
                return Err("repository is not trusted by this node".into());
            }
        }
        if let Some(profile) = &prepare.profile {
            if let Some(max_cores) = self.policy.max_cores {
                let requested = if profile.cpu_cores.is_finite() && profile.cpu_cores > 0.0 {
                    profile.cpu_cores
                } else {
                    1.0
                };
                if requested > f32::from(max_cores) {
                    return Err(format!(
                        "requested CPU ceiling {requested} exceeds this node's {} core limit",
                        max_cores
                    ));
                }
            }
            if let Some(max_memory) = self.policy.max_memory
                && profile.memory > max_memory.0
            {
                return Err(format!(
                    "requested memory ceiling {} exceeds this node's {} byte limit",
                    profile.memory, max_memory.0
                ));
            }
        }
        Ok(())
    }

    fn allowed_egress_hosts(
        &self,
        prepare: &Proto::PrepareWorkspace,
    ) -> Result<Vec<AllowedHost>, String> {
        let profile_hosts = prepare
            .profile
            .as_ref()
            .map(|profile| profile.egress.as_slice())
            .unwrap_or_default();
        self.normalized_egress_hosts(
            profile_hosts
                .iter()
                .map(String::as_str)
                .chain(self.policy.extra_hosts.iter().map(String::as_str)),
        )
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
            "exec {} __fsop",
            shell_quote(&executable.display().to_string())
        );
        let environment = self
            .git_toolchain
            .as_ref()
            .and_then(|toolchain| toolchain.executable.to_str())
            .map(|path| vec![("TAGTEAM_GIT_BIN".into(), path.to_owned())])
            .unwrap_or_default();
        let outcome = self
            .spawn(
                workspace,
                command,
                token,
                limits,
                environment,
                encoded.into_bytes(),
            )
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
        stdin: Vec<u8>,
    ) -> Result<tagteam_sandbox::SandboxOutcome, String> {
        self.ensure_workspace(workspace)?;
        let executable = self
            .executable
            .as_ref()
            .ok_or("node executable unavailable")?;
        let socket = workspace.join("tmp/proxy.sock");
        let id = workspace
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let allowed_hosts = self
            .workspace_egress
            .lock()
            .map_err(|_| "workspace egress policy is unavailable")?
            .get(id)
            .cloned()
            .unwrap_or(self.local_egress_hosts()?);
        let config = ProxyConfig::new(allowed_hosts);
        let (running_proxy, address) = start_workspace_proxy(&socket, config).await?;
        let proxy =
            ProxyEndpoint::new(address, socket).map_err(|_| "sandbox proxy profile is invalid")?;
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
            .map_err(|_| "sandbox profile is invalid")?
            .with_stdin(stdin);
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
        if let Some(toolchain) = &self.git_toolchain {
            profile = profile
                .with_toolchain_root(toolchain.developer_dir.clone())
                .map_err(|_| "sandbox Git toolchain root is invalid")?;
        }
        for (key, value) in environment {
            profile = profile
                .with_environment(key, value)
                .map_err(|_| "command environment is invalid")?;
        }
        #[cfg(target_os = "linux")]
        {
            profile = profile
                .with_environment("TAGTEAM_PROXY_SOCKET".into(), "/run/proxy.sock".into())
                .map_err(|_| "proxy environment is invalid")?;
        }
        let result = spawn_confined(profile, token).await;
        running_proxy.shutdown().await;
        result.map_err(|_| "confined command could not run".to_owned())
    }

    fn local_egress_hosts(&self) -> Result<Vec<AllowedHost>, String> {
        self.normalized_egress_hosts(self.policy.extra_hosts.iter().map(String::as_str))
    }

    /// The product contract always permits the package registries; a task and
    /// the node owner may only add exact hostnames. Starting from the defaults
    /// here keeps a missing preparation profile from silently disabling package
    /// installs while still keeping every request behind the proxy allow-list.
    fn normalized_egress_hosts<'a>(
        &self,
        additional: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<AllowedHost>, String> {
        let mut hosts = Vec::new();
        let mut seen = HashSet::new();
        for raw_host in DEFAULT_EGRESS.iter().copied().chain(additional) {
            if raw_host.len() > 253 || raw_host.trim() != raw_host {
                return Err("egress host is malformed".into());
            }
            let host = AllowedHost::new(raw_host)
                .map_err(|_| format!("egress host is invalid: {raw_host}"))?;
            if seen.insert(host.as_str().to_owned()) {
                if hosts.len() >= MAX_EGRESS_HOSTS {
                    return Err("egress host list exceeds the node limit".into());
                }
                hosts.push(host);
            }
        }
        Ok(hosts)
    }
}

async fn start_workspace_proxy(
    _socket: &Path,
    config: ProxyConfig,
) -> Result<(RunningProxy, SocketAddr), String> {
    #[cfg(target_os = "linux")]
    {
        let (proxy, _outcomes) = UnixProxyServer::start_unix(
            _socket,
            config,
            Arc::new(TokioResolver),
            Arc::new(TcpConnector),
        )
        .await
        .map_err(|error| format!("sandbox egress proxy could not start: {error}"))?;
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), LINUX_PROXY_BRIDGE_PORT);
        Ok((RunningProxy::Unix(proxy), address))
    }
    #[cfg(target_os = "macos")]
    {
        let (proxy, _outcomes) =
            ProxyServer::start(config, Arc::new(TokioResolver), Arc::new(TcpConnector))
                .await
                .map_err(|error| format!("sandbox egress proxy could not start: {error}"))?;
        let address = proxy.addr();
        Ok((RunningProxy::Tcp(proxy), address))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (_socket, config);
        Err("sandbox egress proxy is unsupported on this platform".into())
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

fn cancelled_result(command_id: &str) -> Proto::CommandResult {
    Proto::CommandResult {
        command_id: command_id.to_owned(),
        result: Some(Proto::command_result::Result::Error(Proto::Error {
            code: Proto::ErrorCode::Cancelled as i32,
            message: "command was cancelled".into(),
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

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use super::*;

    struct MarkerBundleSource {
        marker: PathBuf,
        calls: Arc<AtomicUsize>,
    }

    impl BundleSource for MarkerBundleSource {
        fn bundle_bytes(
            &self,
            _reference: &Proto::BlobRef,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + '_>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                fs::write(&self.marker, b"bundle source was reached")
                    .map_err(|_| "marker could not be written".to_owned())?;
                Ok(Vec::new())
            })
        }
    }

    fn prepare_request(repo_id: &str, cores: f32, memory: u64) -> Proto::PrepareWorkspace {
        Proto::PrepareWorkspace {
            bundle: Some(Proto::BlobRef {
                sha256: vec![0; 32],
                size: 0,
            }),
            repo_id: repo_id.into(),
            base: "main".into(),
            branch: "result".into(),
            profile: Some(Proto::SandboxProfile {
                writable: Vec::new(),
                readable: Vec::new(),
                egress: Vec::new(),
                cpu_cores: cores,
                memory,
                pids: 8,
                disk: 1024,
            }),
        }
    }

    #[test]
    fn egress_profile_and_owner_hosts_are_normalized_exact_and_bounded() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal =
            CommandJournal::open(directory.path().join("commands.json")).expect("journal opens");
        let runner = CommandRunner::new(directory.path(), journal, NoBundleSource).with_policy(
            NodePolicy {
                extra_hosts: vec!["Git.Example.".into()],
                ..NodePolicy::default()
            },
            None,
        );
        let mut prepare = prepare_request("repo", 1.0, 1024);
        prepare.profile.as_mut().expect("profile exists").egress =
            vec!["Crates.io".into(), "git.example".into()];
        let hosts = runner
            .allowed_egress_hosts(&prepare)
            .expect("valid exact hosts normalize");
        assert!(hosts.iter().any(|host| host.as_str() == "git.example"));
        for default_host in DEFAULT_EGRESS {
            assert!(hosts.iter().any(|host| host.as_str() == *default_host));
        }

        for invalid in [
            "*.example.com",
            "127.0.0.1",
            "example.com:443",
            " example.com",
        ] {
            prepare.profile.as_mut().expect("profile exists").egress = vec![invalid.into()];
            assert!(runner.allowed_egress_hosts(&prepare).is_err(), "{invalid}");
        }

        prepare.profile.as_mut().expect("profile exists").egress = (0..=MAX_EGRESS_HOSTS)
            .map(|index| format!("host{index}.example"))
            .collect();
        assert!(runner.allowed_egress_hosts(&prepare).is_err());
    }

    #[tokio::test]
    async fn per_command_proxy_shutdown_removes_its_socket() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let socket = directory.path().join("proxy.sock");
        let config = ProxyConfig::new(vec![AllowedHost::new("allowed.example").expect("host")]);
        let (server, address) = start_workspace_proxy(&socket, config)
            .await
            .expect("real egress proxy starts");
        assert!(address.ip().is_loopback());
        #[cfg(target_os = "linux")]
        assert!(fs::symlink_metadata(&socket).is_ok());
        server.shutdown().await;
        #[cfg(target_os = "linux")]
        assert!(!socket.exists());
    }

    #[test]
    fn local_policy_rejects_oversized_profiles_and_untrusted_repositories() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal =
            CommandJournal::open(directory.path().join("commands.json")).expect("journal opens");
        let policy = NodePolicy {
            max_cores: Some(2),
            max_memory: Some(Bytes(1024)),
            ..NodePolicy::default()
        };
        let runner = CommandRunner::new(directory.path(), journal, NoBundleSource)
            .with_policy(policy, Some(vec!["a".repeat(40)]));

        assert!(
            runner
                .check_prepare_policy(&prepare_request(&"a".repeat(40), 2.0, 1024))
                .is_ok()
        );
        assert!(
            runner
                .check_prepare_policy(&prepare_request(&"b".repeat(40), 1.0, 1024))
                .is_err()
        );
        assert!(
            runner
                .check_prepare_policy(&prepare_request(&"a".repeat(40), 3.0, 1024))
                .is_err()
        );
        assert!(
            runner
                .check_prepare_policy(&prepare_request(&"a".repeat(40), 1.0, 2048))
                .is_err()
        );
    }

    #[test]
    fn an_empty_trust_list_denies_every_repository() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal =
            CommandJournal::open(directory.path().join("commands.json")).expect("journal opens");
        let runner = CommandRunner::new(directory.path(), journal, NoBundleSource)
            .with_policy(NodePolicy::default(), Some(Vec::new()));
        assert!(
            runner
                .check_prepare_policy(&prepare_request(&"a".repeat(40), 1.0, 1024))
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_prevents_execution_and_journals_result() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal =
            CommandJournal::open(directory.path().join("commands.json")).expect("journal opens");
        let marker = directory.path().join("executed");
        let calls = Arc::new(AtomicUsize::new(0));
        let source = MarkerBundleSource {
            marker: marker.clone(),
            calls: Arc::clone(&calls),
        };
        let runner = CommandRunner::new(directory.path(), journal.clone(), source);
        let id = "cancel-before-dispatch";

        // Simulate a node restart after Cancel but before the command arrives.
        // The durable marker, not an in-memory entry, must suppress execution.
        runner.cancel(id);
        drop(runner);
        let runner = CommandRunner::new(
            directory.path(),
            journal.clone(),
            MarkerBundleSource {
                marker: marker.clone(),
                calls: Arc::clone(&calls),
            },
        );
        let (sender, mut messages) = SessionSender::test_pair(4);
        runner.dispatch(
            Proto::Command {
                command_id: id.into(),
                subtask_id: "cancel-before-dispatch".into(),
                workspace: "cancel-before-dispatch".into(),
                op: Some(Proto::command::Op::Prepare(Proto::PrepareWorkspace {
                    bundle: Some(Proto::BlobRef {
                        sha256: vec![0x11; 32],
                        size: 1,
                    }),
                    repo_id: "repository".into(),
                    base: "main".into(),
                    branch: "result".into(),
                    profile: None,
                })),
            },
            sender,
        );

        let ack = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match journal.begin(id).expect("journal remains available") {
                    BeginCommand::Completed(ack) => break ack,
                    BeginCommand::Execute | BeginCommand::InProgress => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        })
        .await
        .expect("cancelled command must complete its journal receipt");

        let journaled = ack
            .result
            .as_ref()
            .and_then(|result| result.result.as_ref());
        assert!(
            matches!(
                journaled,
                Some(Proto::command_result::Result::Error(error))
                    if error.code == Proto::ErrorCode::Cancelled as i32
            ),
            "cancellation must be journaled as a terminal error: {ack:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "cancelled command must not reach execution"
        );
        assert!(
            !marker.exists(),
            "cancelled command must not run a side effect"
        );

        let sent = tokio::time::timeout(Duration::from_secs(2), messages.recv())
            .await
            .expect("cancelled result must be sent")
            .expect("session channel remains open");
        let Some(Proto::node_message::Msg::Result(result)) = sent.msg else {
            panic!("expected a command result message");
        };
        assert_eq!(result.command_id, id);
        assert!(matches!(
            result.result,
            Some(Proto::command_result::Result::Error(error))
                if error.code == Proto::ErrorCode::Cancelled as i32
        ));

        for _ in 0..100 {
            if runner
                .running
                .lock()
                .expect("cancellation map remains usable")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            runner
                .running
                .lock()
                .expect("cancellation map remains usable")
                .is_empty(),
            "terminal command must release its cancellation entry"
        );
    }
}
