//! Builds OS confinement policies and supervises bounded command execution.
//!
//! The sandbox owns per-command policy, process groups, and resource observation;
//! workspace provisioning and the allow-list proxy remain node responsibilities.
//! Commands inherit no host environment and are killed as a group on every limit.

use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    fs,
    io::{self, Read},
    net::SocketAddr,
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use sysinfo::{Pid as SystemPid, ProcessesToUpdate, System};
use tagteam_core::{Bytes, Limits};
use thiserror::Error;

/// Maximum combined stdout and stderr retained, even if a caller raises `Limits::max_output`.
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_WORKSPACE_ENTRIES: usize = 100_000;
const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const SAFE_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Failures while validating a profile, building a platform launcher, or supervising a child.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// A required path or profile value is unsafe or invalid.
    #[error("invalid sandbox profile: {0}")]
    InvalidProfile(String),
    /// The current operating system is not supported by this crate.
    #[error("sandbox runtime supports macOS and Linux only")]
    UnsupportedPlatform,
    /// A required host utility could not be started.
    #[error("could not start sandbox command: {0}")]
    Spawn(#[source] io::Error),
    /// A child process operation failed.
    #[error("sandbox process operation failed: {0}")]
    Process(#[source] io::Error),
    /// The workspace usage watcher could not inspect the workspace.
    #[error("could not inspect sandbox workspace: {0}")]
    Workspace(#[source] io::Error),
}

/// A loopback TCP proxy address and its node-owned Unix socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEndpoint {
    /// Loopback TCP address used by macOS Seatbelt policy.
    pub address: SocketAddr,
    /// Host path to the proxy Unix socket, bind-mounted inside Linux sandboxes.
    pub socket_path: PathBuf,
}

impl ProxyEndpoint {
    /// Creates an endpoint, requiring a loopback address, nonzero port, and absolute socket path.
    pub fn new(address: SocketAddr, socket_path: PathBuf) -> Result<Self, SandboxError> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(SandboxError::InvalidProfile(
                "proxy address must use a nonzero loopback port".into(),
            ));
        }
        validate_absolute_path(&socket_path, "proxy socket")?;
        reject_symlink_leaf(&socket_path)?;
        Ok(Self {
            address,
            socket_path,
        })
    }
}

/// Host resource ceilings and the polling interval used by the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Maximum aggregate resident memory of the command and its descendants.
    pub memory: Bytes,
    /// Maximum number of live processes in the command tree.
    pub processes: NonZeroU32,
    /// Maximum total bytes stored below the workspace, without following symlinks.
    pub disk: Bytes,
    /// Sampling interval for memory, process count, and disk usage.
    pub sample_interval: Duration,
}

impl ResourceLimits {
    /// Creates resource limits and rejects a zero sampling interval.
    pub fn new(
        memory: Bytes,
        processes: NonZeroU32,
        disk: Bytes,
        sample_interval: Duration,
    ) -> Result<Self, SandboxError> {
        if sample_interval.is_zero() {
            return Err(SandboxError::InvalidProfile(
                "resource sampling interval must be nonzero".into(),
            ));
        }
        Ok(Self {
            memory,
            processes,
            disk,
            sample_interval,
        })
    }
}

/// Validated inputs for one confined shell command.
#[derive(Debug, Clone)]
pub struct SandboxProfile {
    workspace: PathBuf,
    repo: PathBuf,
    command: String,
    proxy: ProxyEndpoint,
    execution_limits: Limits,
    resource_limits: ResourceLimits,
    toolchain_roots: Vec<PathBuf>,
    environment: Vec<(String, String)>,
    nice_hint: NiceHint,
    job_hint: JobHint,
}

/// A validated process niceness value; larger values yield CPU priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NiceHint(u8);

impl NiceHint {
    /// Creates a nonnegative Unix niceness hint from 0 through 19.
    pub fn new(value: u8) -> Result<Self, SandboxError> {
        if value > 19 {
            return Err(SandboxError::InvalidProfile(
                "nice hint must be between 0 and 19".into(),
            ));
        }
        Ok(Self(value))
    }
}

/// A validated number of CPU jobs used to derive the systemd CPU quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobHint(NonZeroU32);

impl JobHint {
    /// Creates a positive CPU job count; one job maps to a 100% systemd quota.
    #[must_use]
    pub const fn new(value: NonZeroU32) -> Self {
        Self(value)
    }
}

impl SandboxProfile {
    /// Validates and canonicalizes a workspace before any process is started.
    ///
    /// `workspace` is the workspace root containing `repo`, `home`, and `tmp`;
    /// when no `repo` child exists, the workspace itself is treated as the repo.
    pub fn new(
        workspace: impl Into<PathBuf>,
        command: impl Into<String>,
        proxy: ProxyEndpoint,
        execution_limits: Limits,
        resource_limits: ResourceLimits,
    ) -> Result<Self, SandboxError> {
        if resource_limits.sample_interval.is_zero() {
            return Err(SandboxError::InvalidProfile(
                "resource sampling interval must be nonzero".into(),
            ));
        }
        let workspace = canonical_directory(&workspace.into(), "workspace")?;
        ensure_workspace_directory(&workspace, "repo")?;
        ensure_workspace_directory(&workspace, "home")?;
        ensure_workspace_directory(&workspace, "tmp")?;
        let candidate_repo = workspace.join("repo");
        let repo = canonical_directory(&candidate_repo, "workspace repo")?;
        let command = command.into();
        if command.is_empty() || command.contains('\0') {
            return Err(SandboxError::InvalidProfile(
                "command must not be empty or contain NUL".into(),
            ));
        }
        Ok(Self {
            workspace,
            repo,
            command,
            proxy,
            execution_limits,
            resource_limits,
            toolchain_roots: Vec::new(),
            environment: Vec::new(),
            nice_hint: NiceHint(10),
            job_hint: JobHint(NonZeroU32::new(4).ok_or_else(|| {
                SandboxError::InvalidProfile("default job hint cannot be zero".into())
            })?),
        })
    }

    /// Adds a read-only toolchain tree after canonicalizing it and rejecting symlink escapes.
    pub fn with_toolchain_root(mut self, path: PathBuf) -> Result<Self, SandboxError> {
        let root = canonical_directory(&path, "toolchain root")?;
        if self.toolchain_roots.contains(&root) {
            return Err(SandboxError::InvalidProfile(
                "toolchain root was already added".into(),
            ));
        }
        self.toolchain_roots.push(root);
        Ok(self)
    }

    /// Sets the lower scheduling priority hint used for the command process.
    pub fn with_nice_hint(mut self, hint: NiceHint) -> Self {
        self.nice_hint = hint;
        self
    }

    /// Sets the CPU job hint used to derive the systemd CPU quota.
    pub fn with_job_hint(mut self, hint: JobHint) -> Self {
        self.job_hint = hint;
        self
    }

    /// Adds one explicitly requested environment value, excluding host and loader controls.
    pub fn with_environment(mut self, key: String, value: String) -> Result<Self, SandboxError> {
        if self.environment.len() >= MAX_ENVIRONMENT_ENTRIES
            || !valid_environment_key(&key)
            || is_reserved_environment_key(&key)
            || value.contains('\0')
        {
            return Err(SandboxError::InvalidProfile(format!(
                "environment variable {key:?} is invalid or reserved"
            )));
        }
        if self
            .environment
            .iter()
            .any(|(existing, _)| existing == &key)
        {
            return Err(SandboxError::InvalidProfile(format!(
                "environment variable {key:?} was already set"
            )));
        }
        self.environment.push((key, value));
        Ok(self)
    }
}

/// The immutable token observed by a running sandbox.
#[derive(Debug, Clone)]
pub struct CancellationToken(Arc<AtomicBool>);

/// The handle that requests cancellation of all commands using its token.
#[derive(Debug, Clone)]
pub struct CancellationHandle(Arc<AtomicBool>);

impl CancellationToken {
    /// Creates a cancellation handle and its matching observation token.
    #[must_use]
    pub fn new() -> (CancellationHandle, Self) {
        let state = Arc::new(AtomicBool::new(false));
        (CancellationHandle(Arc::clone(&state)), Self(state))
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl CancellationHandle {
    /// Requests cancellation; repeated calls are harmless.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A Linux systemd-run and bubblewrap invocation, ready to pass to `Command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxLaunchPlan {
    /// Executable used for the outer resource-control scope.
    pub program: PathBuf,
    /// Full argument vector for `program`, including the `-- bwrap` separator.
    pub args: Vec<OsString>,
    /// Bubblewrap arguments after the `bwrap` executable.
    pub bwrap_args: Vec<OsString>,
}

/// Why the supervisor stopped an otherwise running command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimitKind {
    /// Aggregate descendant resident memory exceeded the configured cap.
    Memory,
    /// The command tree exceeded the configured process count.
    Processes,
    /// Files below the workspace exceeded the configured disk cap.
    Disk,
}

/// Bounded output and process result for one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOutcome {
    /// Normal exit code, if the command exited normally.
    pub exit_code: Option<i32>,
    /// Unix signal number, if the command was terminated by a signal.
    pub signal: Option<i32>,
    /// Whether the command exceeded the wall-clock command timeout.
    pub timed_out: bool,
    /// Whether an external cancellation request stopped the command.
    pub cancelled: bool,
    /// Resource ceiling that stopped the command, if any.
    pub resource_limit: Option<ResourceLimitKind>,
    /// Captured stdout in head-plus-tail form when output was truncated.
    pub stdout: Vec<u8>,
    /// Captured stderr in head-plus-tail form when output was truncated.
    pub stderr: Vec<u8>,
    /// Whether combined stdout and stderr exceeded the capture bound.
    pub truncated: bool,
}

/// Resource measurements sampled for one sandbox command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceUsage {
    /// Aggregate resident memory in bytes.
    pub memory: Bytes,
    /// Live process count in the command tree.
    pub processes: u32,
    /// Workspace file and symlink storage in bytes.
    pub disk: Bytes,
}

/// Returns the first exceeded ceiling; values equal to a ceiling remain permitted.
#[must_use]
pub fn evaluate_resource_limits(
    usage: ResourceUsage,
    limits: &ResourceLimits,
) -> Option<ResourceLimitKind> {
    if usage.processes > limits.processes.get() {
        Some(ResourceLimitKind::Processes)
    } else if usage.memory.0 > limits.memory.0 {
        Some(ResourceLimitKind::Memory)
    } else if usage.disk.0 > limits.disk.0 {
        Some(ResourceLimitKind::Disk)
    } else {
        None
    }
}

/// Builds a Seatbelt policy that grants workspace access and proxy-only network egress.
///
/// Ancestor directories receive metadata-only access so a sandboxed process can
/// resolve its canonical working directory without gaining read access to the
/// owner's files. Executable mappings are limited to system, toolchain and
/// workspace trees.
pub fn build_seatbelt_profile(profile: &SandboxProfile) -> Result<String, SandboxError> {
    let mut policy = String::from(
        "(version 1)\n(deny default)\n\
         (allow process-fork)\n\
         (allow signal (target same-sandbox))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup (global-name \"com.apple.system.logger\"))\n\
         (allow file-read* (literal \"/\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/System\")\n\
           (subpath \"/Library/Developer/CommandLineTools\") (subpath \"/opt/homebrew\")\n\
           (literal \"/dev/urandom\"))\n\
         (allow file-read* file-write* (literal \"/dev/null\"))\n",
    );
    push_seatbelt_ancestor_metadata(&mut policy, &profile.workspace)?;
    for toolchain in &profile.toolchain_roots {
        push_seatbelt_ancestor_metadata(&mut policy, toolchain)?;
    }
    for toolchain in &profile.toolchain_roots {
        let path = sbpl_string(toolchain.as_os_str())?;
        policy.push_str(&format!(
            "(allow file-read* file-map-executable process-exec (subpath {path}))\n"
        ));
    }
    let workspace = sbpl_string(profile.workspace.as_os_str())?;
    policy.push_str(&format!(
        "(allow file-read* file-write* (subpath {workspace}))\n\
         (allow file-map-executable process-exec (subpath {workspace}))\n"
    ));
    policy.push_str(
        "(allow file-map-executable process-exec (subpath \"/usr\") (subpath \"/bin\")\n\
           (subpath \"/System\") (subpath \"/Library/Developer/CommandLineTools\")\n\
           (subpath \"/opt/homebrew\"))\n",
    );
    // Seatbelt rejects numeric IP literals; the family selector keeps localhost
    // from matching the other loopback family while retaining the exact port.
    let proxy_family = if profile.proxy.address.is_ipv4() {
        "ip4"
    } else {
        "ip6"
    };
    let proxy_host = format!("localhost:{}", profile.proxy.address.port());
    policy.push_str(&format!(
        "(allow network-outbound (remote {proxy_family} {}))\n",
        sbpl_string(std::ffi::OsStr::new(&proxy_host))?
    ));
    Ok(policy)
}

fn push_seatbelt_ancestor_metadata(policy: &mut String, path: &Path) -> Result<(), SandboxError> {
    for ancestor in path.ancestors().skip(1) {
        policy.push_str(&format!(
            "(allow file-read-metadata (literal {}))\n",
            sbpl_string(ancestor.as_os_str())?
        ));
    }
    Ok(())
}

/// Builds the Linux `systemd-run --scope ... bwrap` argv without invoking a shell.
pub fn build_linux_launch_plan(profile: &SandboxProfile) -> Result<LinuxLaunchPlan, SandboxError> {
    validate_workspace_proxy_socket(profile)?;
    let mut bwrap_args = os_args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--tmpfs",
        "/",
        "--dir",
        "/ws",
        "--dir",
        "/toolchains",
        "--ro-bind",
        "/usr",
        "/usr",
        "--ro-bind",
        "/etc/ssl",
        "/etc/ssl",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib",
        "/lib64",
    ]);
    for (index, toolchain) in profile.toolchain_roots.iter().enumerate() {
        bwrap_args.extend(os_args(["--dir"]));
        bwrap_args.push(OsString::from(format!("/toolchains/{index}")));
        bwrap_args.extend([
            OsString::from("--ro-bind"),
            toolchain.as_os_str().to_owned(),
            OsString::from(format!("/toolchains/{index}")),
        ]);
    }
    bwrap_args.extend(os_args([
        "--bind",
        profile.workspace.to_str().ok_or_else(|| {
            SandboxError::InvalidProfile("workspace path is not valid UTF-8".into())
        })?,
        "/ws",
        "--chdir",
        "/ws/repo",
        "--setenv",
        "HOME",
        "/ws/home",
        "--setenv",
        "TMPDIR",
        "/ws/tmp",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/run",
        "--bind",
    ]));
    bwrap_args.push(profile.proxy.socket_path.as_os_str().to_owned());
    bwrap_args.extend(os_args([
        "/run/proxy.sock",
        "--setenv",
        "HTTPS_PROXY",
        "http://unix:/run/proxy.sock",
        "--setenv",
        "HTTP_PROXY",
        "http://unix:/run/proxy.sock",
        "--setenv",
        "PATH",
    ]));
    let mut toolchain_path = profile
        .toolchain_roots
        .iter()
        .enumerate()
        .map(|(index, _)| format!("/toolchains/{index}"))
        .collect::<Vec<_>>();
    toolchain_path.push(SAFE_PATH.into());
    bwrap_args.push(OsString::from(toolchain_path.join(":")));
    for (key, value) in &profile.environment {
        bwrap_args.extend(os_args(["--setenv", key, value]));
    }
    bwrap_args.extend(os_args(["--", "/usr/bin/nice", "-n"]));
    bwrap_args.push(OsString::from(profile.nice_hint.0.to_string()));
    bwrap_args.extend(os_args(["/bin/bash", "-c"]));
    bwrap_args.push(OsString::from(&profile.command));

    let memory_max = profile.resource_limits.memory.0.to_string();
    let tasks_max = profile.resource_limits.processes.get().to_string();
    let cpu_quota = u64::from(profile.job_hint.0.get()).saturating_mul(100);
    let mut args = os_args(["--user", "--scope", "-p"]);
    args.push(OsString::from(format!("CPUQuota={cpu_quota}%")));
    args.extend(os_args(["-p"]));
    args.push(OsString::from(format!("MemoryMax={memory_max}")));
    args.extend(os_args(["-p"]));
    args.push(OsString::from(format!("TasksMax={tasks_max}")));
    args.extend(os_args(["--", "/usr/bin/bwrap"]));
    args.extend(bwrap_args.iter().cloned());
    Ok(LinuxLaunchPlan {
        program: PathBuf::from("/usr/bin/systemd-run"),
        args,
        bwrap_args,
    })
}

/// Runs a command inside Seatbelt or bubblewrap on Tokio's blocking pool.
///
/// Dropping this future requests cancellation of the blocking supervisor; the
/// external token can independently stop a command while the future is awaited.
pub async fn spawn_confined(
    profile: SandboxProfile,
    token: CancellationToken,
) -> Result<SandboxOutcome, SandboxError> {
    let task_cancelled = Arc::new(AtomicBool::new(false));
    let mut drop_guard = AsyncCancellationGuard {
        cancelled: Arc::clone(&task_cancelled),
        armed: true,
    };
    let worker = tokio::task::spawn_blocking(move || {
        spawn_confined_blocking(profile, token, task_cancelled)
    });
    let result = worker.await.map_err(|error| {
        SandboxError::Process(io::Error::other(format!(
            "sandbox supervisor task failed: {error}"
        )))
    })?;
    drop_guard.armed = false;
    result
}

#[derive(Debug)]
struct AsyncCancellationGuard {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for AsyncCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

fn spawn_confined_blocking(
    profile: SandboxProfile,
    token: CancellationToken,
    task_cancelled: Arc<AtomicBool>,
) -> Result<SandboxOutcome, SandboxError> {
    let mut command = platform_command(&profile)?;
    command
        .env_clear()
        .env("HOME", host_fake_home(&profile))
        .env("TMPDIR", host_fake_tmp(&profile))
        .env("PATH", SAFE_PATH)
        .env("HTTPS_PROXY", proxy_url(&profile))
        .env("HTTP_PROXY", proxy_url(&profile))
        .current_dir(&profile.repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &profile.environment {
        command.env(key, value);
    }
    configure_process_group(&mut command);
    let mut child = command.spawn().map_err(SandboxError::Spawn)?;
    let pid = child.id();
    let mut group_guard = ChildGroupGuard::new(pid);
    let stdout = child.stdout.take().ok_or_else(|| {
        SandboxError::Process(io::Error::other("sandbox stdout pipe was not created"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        SandboxError::Process(io::Error::other("sandbox stderr pipe was not created"))
    })?;
    let capture_limit = usize::try_from(profile.execution_limits.max_output.0)
        .unwrap_or(usize::MAX)
        .min(MAX_CAPTURE_BYTES);
    let capture = Arc::new(Mutex::new(OutputCapture::new(capture_limit)));
    let stdout_reader = spawn_reader(stdout, Arc::clone(&capture), Stream::Stdout);
    let stderr_reader = spawn_reader(stderr, Arc::clone(&capture), Stream::Stderr);
    let started = Instant::now();
    let timeout = profile.execution_limits.command_timeout;
    let mut system = System::new();
    let mut stopped_by = None;
    let mut next_sample = started + profile.resource_limits.sample_interval;

    let status = loop {
        if let Some(exit) = child.try_wait().map_err(SandboxError::Process)? {
            group_guard.terminate(&mut system);
            break exit;
        }
        if token.is_cancelled() || task_cancelled.load(Ordering::Acquire) {
            stopped_by = Some(StopReason::Cancelled);
        } else if started.elapsed() >= timeout {
            stopped_by = Some(StopReason::Timeout);
        } else if Instant::now() >= next_sample {
            system.refresh_processes(ProcessesToUpdate::All, true);
            let process_usage = process_tree_usage(&system, pid);
            let disk = workspace_size(&profile.workspace).map_err(SandboxError::Workspace)?;
            let usage = ResourceUsage {
                memory: Bytes(process_usage.memory),
                processes: u32::try_from(process_usage.count).unwrap_or(u32::MAX),
                disk: Bytes(disk),
            };
            if let Some(limit) = evaluate_resource_limits(usage, &profile.resource_limits) {
                stopped_by = Some(StopReason::Resource(limit));
            }
            next_sample = Instant::now() + profile.resource_limits.sample_interval;
        }
        if stopped_by.is_some() {
            group_guard.terminate(&mut system);
            break child.wait().map_err(SandboxError::Process)?;
        }
        let until_sample = next_sample.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_millis(15).min(until_sample.max(Duration::from_millis(1))));
    };

    join_reader(stdout_reader)?;
    join_reader(stderr_reader)?;
    let mut output = capture
        .lock()
        .map_err(|_| SandboxError::Process(io::Error::other("output capture lock poisoned")))?;
    let (stdout, stderr, truncated) = output.finish();
    let (timed_out, cancelled, resource_limit) = match stopped_by {
        Some(StopReason::Timeout) => (true, false, None),
        Some(StopReason::Cancelled) => (false, true, None),
        Some(StopReason::Resource(kind)) => (false, false, Some(kind)),
        None => (false, false, None),
    };
    let (exit_code, signal) = exit_fields(Some(status));
    Ok(SandboxOutcome {
        exit_code,
        signal,
        timed_out,
        cancelled,
        resource_limit,
        stdout,
        stderr,
        truncated,
    })
}

#[derive(Debug)]
struct ChildGroupGuard {
    root: u32,
    armed: bool,
}

impl ChildGroupGuard {
    fn new(root: u32) -> Self {
        Self { root, armed: true }
    }

    fn terminate(&mut self, system: &mut System) {
        if self.armed {
            kill_process_tree(system, self.root);
            self.armed = false;
        }
    }
}

impl Drop for ChildGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut system = System::new();
            kill_process_tree(&mut system, self.root);
            self.armed = false;
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StopReason {
    Timeout,
    Cancelled,
    Resource(ResourceLimitKind),
}

#[derive(Debug, Default)]
struct ProcessTreeUsage {
    count: usize,
    memory: u64,
}

fn process_tree_usage(system: &System, root: u32) -> ProcessTreeUsage {
    let root = SystemPid::from_u32(root);
    let mut included = HashMap::from([(root, true)]);
    loop {
        let mut changed = false;
        for (pid, process) in system.processes() {
            if process
                .parent()
                .is_some_and(|parent| included.contains_key(&parent))
                && !included.contains_key(pid)
            {
                included.insert(*pid, true);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut usage = ProcessTreeUsage::default();
    for pid in included.keys() {
        if let Some(process) = system.process(*pid) {
            usage.count = usage.count.saturating_add(1);
            usage.memory = usage.memory.saturating_add(process.memory());
        }
    }
    usage
}

fn kill_process_tree(system: &mut System, root: u32) {
    let root_system = SystemPid::from_u32(root);
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut descendants = Vec::new();
    let mut included = HashMap::from([(root_system, true)]);
    loop {
        let mut changed = false;
        for (pid, process) in system.processes() {
            if process
                .parent()
                .is_some_and(|parent| included.contains_key(&parent))
                && !included.contains_key(pid)
            {
                included.insert(*pid, true);
                descendants.push(*pid);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for pid in descendants.into_iter().rev() {
        if let Ok(raw_pid) = i32::try_from(pid.as_u32()) {
            let _ = kill(Pid::from_raw(raw_pid), Signal::SIGKILL);
        }
    }
    if let Ok(raw_pid) = i32::try_from(root) {
        let pid = Pid::from_raw(raw_pid);
        let _ = killpg(pid, Signal::SIGKILL);
        let _ = kill(pid, Signal::SIGKILL);
    }
}

fn workspace_size(path: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![path.to_path_buf()];
    let mut entries_seen = 0_usize;
    while let Some(directory) = pending.pop() {
        if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > MAX_WORKSPACE_ENTRIES {
                return Ok(u64::MAX);
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                total = total.saturating_add(metadata.len());
            } else if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct OutputCapture {
    limit: usize,
    seen: usize,
    head: Vec<(Stream, Vec<u8>)>,
    tail: VecDeque<(Stream, u8)>,
}

impl OutputCapture {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            seen: 0,
            head: Vec::new(),
            tail: VecDeque::new(),
        }
    }

    fn push(&mut self, stream: Stream, bytes: &[u8]) {
        let head_limit = self.limit.div_ceil(2);
        for byte in bytes {
            let offset = self.seen;
            self.seen = self.seen.saturating_add(1);
            if offset < head_limit {
                match self.head.last_mut() {
                    Some((last_stream, data)) if matches_stream(*last_stream, stream) => {
                        data.push(*byte);
                    }
                    _ => self.head.push((stream, vec![*byte])),
                }
            } else if self.limit > head_limit {
                self.tail.push_back((stream, *byte));
                while self.tail.len() > self.limit - head_limit {
                    self.tail.pop_front();
                }
            }
        }
    }

    fn finish(&mut self) -> (Vec<u8>, Vec<u8>, bool) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for (stream, bytes) in self.head.drain(..) {
            append_stream(&mut stdout, &mut stderr, stream, &bytes);
        }
        for (stream, byte) in self.tail.drain(..) {
            append_stream(
                &mut stdout,
                &mut stderr,
                stream,
                std::slice::from_ref(&byte),
            );
        }
        (stdout, stderr, self.seen > self.limit)
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    output: Arc<Mutex<OutputCapture>>,
    stream: Stream,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            output
                .lock()
                .map_err(|_| io::Error::other("output capture lock poisoned"))?
                .push(stream, &buffer[..count]);
        }
    })
}

fn join_reader(handle: thread::JoinHandle<io::Result<()>>) -> Result<(), SandboxError> {
    handle
        .join()
        .map_err(|_| SandboxError::Process(io::Error::other("output reader thread panicked")))?
        .map_err(SandboxError::Process)
}

fn append_stream(stdout: &mut Vec<u8>, stderr: &mut Vec<u8>, stream: Stream, bytes: &[u8]) {
    match stream {
        Stream::Stdout => stdout.extend_from_slice(bytes),
        Stream::Stderr => stderr.extend_from_slice(bytes),
    }
}

fn matches_stream(left: Stream, right: Stream) -> bool {
    matches!(
        (left, right),
        (Stream::Stdout, Stream::Stdout) | (Stream::Stderr, Stream::Stderr)
    )
}

fn platform_command(profile: &SandboxProfile) -> Result<Command, SandboxError> {
    if cfg!(target_os = "macos") {
        prepare_fake_dirs(profile)?;
        let policy = build_seatbelt_profile(profile)?;
        // Set priority before entering Seatbelt: sandboxed processes cannot call
        // setpriority, so wrapping the sandbox from outside preserves the hint.
        let mut command = Command::new("/usr/bin/nice");
        command.args([
            OsString::from("-n"),
            OsString::from(profile.nice_hint.0.to_string()),
            OsString::from("/usr/bin/sandbox-exec"),
            OsString::from("-p"),
            OsString::from(policy),
            OsString::from("/bin/bash"),
            OsString::from("-c"),
        ]);
        command.arg(&profile.command);
        Ok(command)
    } else if cfg!(target_os = "linux") {
        let plan = build_linux_launch_plan(profile)?;
        let mut command = Command::new(plan.program);
        command.args(plan.args);
        Ok(command)
    } else {
        Err(SandboxError::UnsupportedPlatform)
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

fn prepare_fake_dirs(profile: &SandboxProfile) -> Result<(), SandboxError> {
    for path in [host_fake_home(profile), host_fake_tmp(profile)] {
        fs::create_dir_all(&path).map_err(SandboxError::Workspace)?;
        let canonical = fs::canonicalize(&path).map_err(SandboxError::Workspace)?;
        if !canonical.starts_with(&profile.workspace) {
            return Err(SandboxError::InvalidProfile(
                "fake HOME and TMPDIR must remain inside the workspace".into(),
            ));
        }
    }
    Ok(())
}

fn host_fake_home(profile: &SandboxProfile) -> PathBuf {
    profile.workspace.join("home")
}

fn host_fake_tmp(profile: &SandboxProfile) -> PathBuf {
    profile.workspace.join("tmp")
}

fn proxy_url(profile: &SandboxProfile) -> String {
    format!("http://{}", profile.proxy.address)
}

fn exit_fields(status: Option<ExitStatus>) -> (Option<i32>, Option<i32>) {
    match status {
        Some(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                (status.code(), status.signal())
            }
            #[cfg(not(unix))]
            {
                (status.code(), None)
            }
        }
        None => (None, None),
    }
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, SandboxError> {
    validate_absolute_path(path, label)?;
    reject_symlink_leaf(path)?;
    let canonical = fs::canonicalize(path).map_err(|error| {
        SandboxError::InvalidProfile(format!("{label} cannot be resolved: {error}"))
    })?;
    if !canonical.is_dir() {
        return Err(SandboxError::InvalidProfile(format!(
            "{label} must be a directory"
        )));
    }
    Ok(canonical)
}

fn ensure_workspace_directory(workspace: &Path, name: &str) -> Result<PathBuf, SandboxError> {
    let path = workspace.join(name);
    reject_symlink_leaf(&path)?;
    if !path.exists() {
        fs::create_dir(&path).map_err(SandboxError::Workspace)?;
    }
    let canonical = canonical_directory(&path, name)?;
    if !canonical.starts_with(workspace) {
        return Err(SandboxError::InvalidProfile(format!(
            "workspace {name} directory resolves outside the workspace"
        )));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn validate_unix_socket(path: &Path) -> Result<(), SandboxError> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        SandboxError::InvalidProfile(format!(
            "proxy socket {} is unavailable: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_socket() {
        Ok(())
    } else {
        Err(SandboxError::InvalidProfile(format!(
            "proxy path {} must be an existing Unix socket",
            path.display()
        )))
    }
}

#[cfg(not(unix))]
fn validate_unix_socket(_path: &Path) -> Result<(), SandboxError> {
    Err(SandboxError::UnsupportedPlatform)
}

fn validate_workspace_proxy_socket(profile: &SandboxProfile) -> Result<(), SandboxError> {
    validate_unix_socket(&profile.proxy.socket_path)?;
    let socket = fs::canonicalize(&profile.proxy.socket_path).map_err(|error| {
        SandboxError::InvalidProfile(format!("proxy socket path cannot be resolved: {error}"))
    })?;
    let tmp =
        fs::canonicalize(host_fake_tmp(profile).as_path()).map_err(SandboxError::Workspace)?;
    if !socket.starts_with(tmp) {
        return Err(SandboxError::InvalidProfile(
            "Linux proxy socket must be inside workspace/tmp".into(),
        ));
    }
    Ok(())
}

fn validate_absolute_path(path: &Path, label: &str) -> Result<(), SandboxError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SandboxError::InvalidProfile(format!(
            "{label} must be absolute and contain no dot segments"
        )));
    }
    Ok(())
}

fn reject_symlink_leaf(path: &Path) -> Result<(), SandboxError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SandboxError::InvalidProfile(
            format!("path {} is a symlink", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SandboxError::InvalidProfile(format!(
            "cannot inspect path {}: {error}",
            path.display()
        ))),
    }
}

fn valid_environment_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_uppercase())
        && chars.all(|character| {
            character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit()
        })
}

fn is_reserved_environment_key(key: &str) -> bool {
    matches!(
        key,
        "HOME"
            | "TMPDIR"
            | "PATH"
            | "SHELL"
            | "BASH_ENV"
            | "ENV"
            | "IFS"
            | "HTTPS_PROXY"
            | "HTTP_PROXY"
            | "ALL_PROXY"
            | "NO_PROXY"
    ) || key.starts_with("DYLD_")
        || key.starts_with("LD_")
        || key.starts_with("BASH_FUNC_")
}

fn sbpl_string(value: &std::ffi::OsStr) -> Result<String, SandboxError> {
    let value = value
        .to_str()
        .ok_or_else(|| SandboxError::InvalidProfile("Seatbelt paths must be valid UTF-8".into()))?;
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            character if character.is_control() => {
                for byte in character.to_string().bytes() {
                    escaped.push_str(&format!("\\{byte:03o}"));
                }
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    Ok(escaped)
}

fn os_args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ProxyEndpoint, ResourceLimitKind, ResourceLimits, ResourceUsage, SandboxProfile,
        build_seatbelt_profile, evaluate_resource_limits,
    };
    use std::{
        fs,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::NonZeroU32,
        time::Duration,
    };
    use tagteam_core::{Bytes, Limits};
    use tempfile::tempdir;

    fn limits() -> ResourceLimits {
        ResourceLimits {
            memory: Bytes(100),
            processes: NonZeroU32::new(4).unwrap(),
            disk: Bytes(1_000),
            sample_interval: Duration::from_millis(50),
        }
    }

    #[test]
    fn resource_limits_allow_measurements_on_each_boundary() {
        let ceilings = limits();
        assert_eq!(
            evaluate_resource_limits(
                ResourceUsage {
                    memory: Bytes(100),
                    processes: 4,
                    disk: Bytes(1_000),
                },
                &ceilings,
            ),
            None
        );
    }

    #[test]
    fn resource_limits_reject_memory_over_boundary() {
        let ceilings = limits();
        assert_eq!(
            evaluate_resource_limits(
                ResourceUsage {
                    memory: Bytes(101),
                    processes: 4,
                    disk: Bytes(1_000),
                },
                &ceilings,
            ),
            Some(ResourceLimitKind::Memory)
        );
    }

    #[test]
    fn resource_limits_reject_process_count_over_boundary() {
        let ceilings = limits();
        assert_eq!(
            evaluate_resource_limits(
                ResourceUsage {
                    memory: Bytes(100),
                    processes: 5,
                    disk: Bytes(1_000),
                },
                &ceilings,
            ),
            Some(ResourceLimitKind::Processes)
        );
    }

    #[test]
    fn resource_limits_reject_disk_over_boundary() {
        let ceilings = limits();
        assert_eq!(
            evaluate_resource_limits(
                ResourceUsage {
                    memory: Bytes(100),
                    processes: 4,
                    disk: Bytes(1_001),
                },
                &ceilings,
            ),
            Some(ResourceLimitKind::Disk)
        );
    }

    #[test]
    fn seatbelt_network_policy_matches_proxy_family_and_port() {
        for (address, expected_rule) in [
            (
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18_765),
                "(allow network-outbound (remote ip4 \"localhost:18765\"))",
            ),
            (
                SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 18_765),
                "(allow network-outbound (remote ip6 \"localhost:18765\"))",
            ),
        ] {
            let workspace = tempdir().unwrap();
            for directory in ["repo", "home", "tmp"] {
                fs::create_dir(workspace.path().join(directory)).unwrap();
            }
            let proxy =
                ProxyEndpoint::new(address, workspace.path().join("tmp/proxy.sock")).unwrap();
            let profile =
                SandboxProfile::new(workspace.path(), "true", proxy, Limits::default(), limits())
                    .unwrap();

            let policy = build_seatbelt_profile(&profile).unwrap();
            let network_rules = policy
                .lines()
                .filter(|line| line.starts_with("(allow network-outbound"))
                .collect::<Vec<_>>();
            assert_eq!(network_rules, [expected_rule]);
            assert!(!policy.contains("remote ip \"localhost:18765\""));
        }
    }
}
