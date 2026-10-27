//! Exercises the real OS boundary and portable policy builders for tagteam-sandbox.
//!
//! Runtime escape cases run only on macOS, where Seatbelt is the selected backend;
//! Linux tests inspect the generated bwrap plan and do not claim bwrap execution.

use std::os::unix::net::UnixListener;
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "macos")]
use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
#[cfg(target_os = "macos")]
use std::io::ErrorKind;
#[cfg(target_os = "macos")]
use std::net::TcpListener;
use tagteam_core::types::{Bytes, Limits};
use tagteam_sandbox::{
    CancellationToken, ProxyEndpoint, ResourceLimits, SandboxProfile, build_linux_launch_plan,
    build_seatbelt_profile, spawn_confined,
};
use tempfile::{Builder, TempDir};

#[cfg(target_os = "macos")]
const CAPTURE_LIMIT: usize = 256 * 1024;

fn test_limits() -> (Limits, ResourceLimits) {
    let execution = Limits {
        command_timeout: Duration::from_secs(10),
        max_output: Bytes::mib(2),
        ..Limits::default()
    };
    let resources = ResourceLimits {
        memory: Bytes::gib(2),
        processes: NonZeroU32::new(64).expect("nonzero process test limit"),
        disk: Bytes::mib(64),
        sample_interval: Duration::from_millis(25),
    };
    (execution, resources)
}

fn workspace(prefix: &str) -> TempDir {
    let root = Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("temporary workspace root");
    populate_workspace(&root);
    root
}

fn workspace_in(parent: &Path, prefix: &str) -> TempDir {
    let root = Builder::new()
        .prefix(prefix)
        .tempdir_in(parent)
        .expect("temporary workspace root");
    populate_workspace(&root);
    root
}

fn populate_workspace(root: &TempDir) {
    for directory in ["repo", "home", "tmp"] {
        fs::create_dir_all(root.path().join(directory)).expect("workspace directory");
    }
}

fn profile(root: &Path, command: &str) -> SandboxProfile {
    let (execution, resources) = test_limits();
    SandboxProfile::new(
        root.to_path_buf(),
        command.to_owned(),
        proxy_endpoint(root),
        execution,
        resources,
    )
    .expect("valid sandbox profile")
}

fn proxy_endpoint(root: &Path) -> ProxyEndpoint {
    let socket_path = root.join("tmp/proxy.sock");
    drop(UnixListener::bind(&socket_path).expect("temporary proxy socket"));
    ProxyEndpoint::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18_765),
        socket_path,
    )
    .expect("loopback proxy endpoint")
}

fn strings(values: &[std::ffi::OsString]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

fn replace_path(value: String, path: &Path, replacement: &str) -> String {
    value.replace(path.to_str().expect("test path is UTF-8"), replacement)
}

#[test]
fn seatbelt_and_argv_preserve_escaped_unusual_paths() {
    let prefix = "tt-\"quoted\" (paren)\nslash\\ café-";
    let short_parent = Path::new("/private/tmp");
    let root = if short_parent.is_dir() {
        workspace_in(short_parent, prefix)
    } else {
        workspace(prefix)
    };
    let toolchain = Builder::new()
        .prefix("toolchain \"(arm64)\n\\ ü-")
        .tempdir()
        .expect("temporary toolchain root");
    let mut sandbox = profile(root.path(), "printf 'safe'");
    sandbox = sandbox
        .with_toolchain_root(toolchain.path().to_path_buf())
        .expect("toolchain root");

    let policy = build_seatbelt_profile(&sandbox).expect("Seatbelt policy");
    let workspace_path = fs::canonicalize(root.path()).expect("canonical workspace");
    let toolchain_path = fs::canonicalize(toolchain.path()).expect("canonical toolchain");
    assert!(policy.contains("\\\"quoted\\\""));
    assert!(policy.contains("\\012"));
    assert!(policy.contains("\\\\"));
    assert!(policy.contains("café"));
    assert!(!policy.contains(workspace_path.to_str().expect("UTF-8 workspace")));
    assert!(!policy.contains(toolchain_path.to_str().expect("UTF-8 toolchain")));

    let plan = build_linux_launch_plan(&sandbox).expect("Linux launch plan");
    assert!(
        plan.bwrap_args
            .iter()
            .any(|arg| arg == workspace_path.as_os_str())
    );
    assert!(
        plan.bwrap_args
            .iter()
            .any(|arg| arg == toolchain_path.as_os_str())
    );
}

#[test]
fn linux_bwrap_argv_matches_the_documented_confinement_contract() {
    let root = workspace("tag-team-linux-snapshot-");
    let toolchain = Builder::new()
        .prefix("toolchain-snapshot-")
        .tempdir()
        .expect("temporary toolchain root");
    let (execution, mut resources) = test_limits();
    resources.memory = Bytes::gib(8);
    resources.processes = NonZeroU32::new(512).unwrap();
    let mut sandbox = SandboxProfile::new(
        root.path().to_path_buf(),
        "printf '%s' \"$ARG\"".to_owned(),
        proxy_endpoint(root.path()),
        execution,
        resources,
    )
    .unwrap();
    sandbox = sandbox
        .with_toolchain_root(toolchain.path().to_path_buf())
        .expect("toolchain root")
        .with_environment("ARG".to_owned(), "quotes ' \" ; $(id)".to_owned())
        .expect("command environment");

    let plan = build_linux_launch_plan(&sandbox).expect("Linux launch plan");
    let mut actual = strings(&plan.args);
    let workspace_canonical = fs::canonicalize(root.path()).expect("canonical workspace");
    let toolchain_canonical = fs::canonicalize(toolchain.path()).expect("canonical toolchain");
    for value in &mut actual {
        *value = replace_path(value.clone(), &workspace_canonical, "$WS");
        *value = replace_path(value.clone(), root.path(), "$WS");
        *value = replace_path(value.clone(), &toolchain_canonical, "$TOOLCHAINS");
    }
    assert_eq!(
        actual,
        [
            "--user",
            "--scope",
            "-p",
            "CPUQuota=400%",
            "-p",
            "MemoryMax=8589934592",
            "-p",
            "TasksMax=512",
            "--",
            "/usr/bin/bwrap",
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
            "--dir",
            "/toolchains/0",
            "--ro-bind",
            "$TOOLCHAINS",
            "/toolchains/0",
            "--bind",
            "$WS",
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
            "$WS/tmp/proxy.sock",
            "/run/proxy.sock",
            "--setenv",
            "HTTPS_PROXY",
            "http://unix:/run/proxy.sock",
            "--setenv",
            "HTTP_PROXY",
            "http://unix:/run/proxy.sock",
            "--setenv",
            "PATH",
            "/toolchains/0:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            "--setenv",
            "ARG",
            "quotes ' \" ; $(id)",
            "--",
            "/usr/bin/nice",
            "-n",
            "10",
            "/bin/bash",
            "-c",
            "printf '%s' \"$ARG\"",
        ]
        .map(str::to_owned)
    );
    assert!(
        plan.args
            .windows(2)
            .any(|pair| pair == ["-p", "MemoryMax=8589934592"])
    );
    assert!(actual.contains(&"--unshare-all".to_owned()));
    assert!(!actual.contains(&"--share-net".to_owned()));
    assert_eq!(plan.program, PathBuf::from("/usr/bin/systemd-run"));
}

#[test]
fn non_loopback_proxy_is_rejected() {
    let result = ProxyEndpoint::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 18_765),
        PathBuf::from("/run/tagteam/proxy.sock"),
    );
    assert!(result.is_err());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_reads_system_binary_and_reads_writes_workspace() {
    let root = workspace("tag-team-macos-workspace-");
    let sandbox = profile(
        root.path(),
        "/bin/cat /usr/bin/true >/dev/null && printf 'workspace-ok' > written.txt && test \"$(/bin/cat written.txt)\" = workspace-ok",
    );
    let (_cancel, token) = CancellationToken::new();
    let result = spawn_confined(sandbox, token)
        .await
        .expect("confined command runs");
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    assert_eq!(
        fs::read(root.path().join("repo/written.txt")).unwrap(),
        b"workspace-ok"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_network_access_is_limited_to_the_configured_proxy_endpoint() {
    let root = workspace("tag-team-macos-network-");
    let denied_listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("non-proxy loopback listener");
    let proxy_listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("proxy loopback listener");
    denied_listener
        .set_nonblocking(true)
        .expect("nonblocking denied listener");
    proxy_listener
        .set_nonblocking(true)
        .expect("nonblocking proxy listener");
    let denied_address = denied_listener
        .local_addr()
        .expect("denied listener address");
    let proxy_address = proxy_listener.local_addr().expect("proxy listener address");
    assert_ne!(denied_address, proxy_address);

    let socket_path = root.path().join("tmp/proxy.sock");
    drop(UnixListener::bind(&socket_path).expect("temporary proxy socket"));
    let proxy = ProxyEndpoint::new(proxy_address, socket_path).expect("proxy endpoint");
    let (mut execution, resources) = test_limits();
    execution.command_timeout = Duration::from_secs(3);
    let sandbox = SandboxProfile::new(
        root.path().to_path_buf(),
        "if { : > \"/dev/tcp/127.0.0.1/$DENIED_PORT\"; } 2>/dev/null; then exit 41; fi; \
         : > \"/dev/tcp/127.0.0.1/$PROXY_PORT\""
            .to_owned(),
        proxy,
        execution,
        resources,
    )
    .expect("valid network test profile")
    .with_environment("DENIED_PORT".to_owned(), denied_address.port().to_string())
    .expect("denied listener port")
    .with_environment("PROXY_PORT".to_owned(), proxy_address.port().to_string())
    .expect("proxy listener port");
    let (_cancel, token) = CancellationToken::new();
    let result = spawn_confined(sandbox, token)
        .await
        .expect("network policy command returns an outcome");

    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    assert_eq!(result.resource_limit, None);
    proxy_listener
        .accept()
        .expect("confined command connected to the configured proxy");
    assert!(matches!(
        denied_listener.accept(),
        Err(error) if error.kind() == ErrorKind::WouldBlock
    ));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_escape_attempts_fail_closed_without_exposing_file_contents() {
    let root = workspace("tag-team-macos-escape-workspace-");
    let owner_home = workspace("tag-team-protected-owner-home-");
    fs::create_dir_all(owner_home.path().join(".ssh")).unwrap();
    fs::create_dir_all(owner_home.path().join("Library/Keychains")).unwrap();
    fs::write(
        owner_home.path().join(".ssh/id_rsa"),
        "protected ssh sentinel",
    )
    .unwrap();
    fs::write(
        owner_home.path().join("Library/Keychains/keychain-db"),
        "protected keychain sentinel",
    )
    .unwrap();

    let outside = Builder::new()
        .prefix("tag-team-outside-workspace-")
        .tempdir()
        .unwrap();
    let outside_file = outside.path().join("outside-secret.txt");
    fs::write(&outside_file, "must remain unread and unchanged").unwrap();
    std::os::unix::fs::symlink(&outside_file, root.path().join("repo/escape-link")).unwrap();

    let mut sandbox = profile(
        root.path(),
        "if /bin/ls -A \"$SSH_DIR\" >/dev/null 2>&1; then exit 11; fi; \
         if /bin/cat \"$KEYCHAIN_FILE\" >/dev/null 2>&1; then exit 12; fi; \
         if { printf x > \"$OUTSIDE_WRITE\"; } 2>/dev/null; then exit 13; fi; \
         if /bin/cat \"$ESCAPE_LINK\" >/dev/null 2>&1; then exit 14; fi; \
         if { printf x > \"$ESCAPE_LINK\"; } 2>/dev/null; then exit 15; fi; \
         exit 0",
    );
    for (key, value) in [
        ("SSH_DIR", owner_home.path().join(".ssh")),
        (
            "KEYCHAIN_FILE",
            owner_home.path().join("Library/Keychains/keychain-db"),
        ),
        ("OUTSIDE_WRITE", outside.path().join("outside-write.txt")),
        ("ESCAPE_LINK", root.path().join("repo/escape-link")),
    ] {
        sandbox = sandbox
            .with_environment(key.to_owned(), value.to_string_lossy().into_owned())
            .expect("test path environment");
    }

    let (_cancel, token) = CancellationToken::new();
    let result = spawn_confined(sandbox, token)
        .await
        .expect("sandbox command starts");
    assert_eq!(
        result.exit_code,
        Some(0),
        "sandbox failure diagnostic (fixture contents are redirected): signal={:?}, timeout={}, cancelled={}, resource={:?}, stderr={}",
        result.signal,
        result.timed_out,
        result.cancelled,
        result.resource_limit,
        String::from_utf8_lossy(&result.stderr),
    );
    assert_eq!(
        fs::read(&outside_file).unwrap(),
        b"must remain unread and unchanged"
    );
    assert!(!outside.path().join("outside-write.txt").exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_timeout_kills_child_and_grandchild_and_keeps_partial_output() {
    let root = workspace("tag-team-macos-timeout-");
    let (mut execution, resources) = test_limits();
    execution.command_timeout = Duration::from_millis(900);
    let sandbox = SandboxProfile::new(
        root.path().to_path_buf(),
        "sleep 60 & direct=$!; (sleep 60 & echo $! > \"$TMPDIR/grandchild.pid\"; wait) & middle=$!; printf 'partial:%s:%s\\n' \"$direct\" \"$middle\"; sleep 60".to_owned(),
        proxy_endpoint(root.path()),
        execution,
        resources,
    )
    .unwrap();
    let (_cancel, token) = CancellationToken::new();
    let result = spawn_confined(sandbox, token)
        .await
        .expect("timed command returns an outcome");

    assert!(result.timed_out);
    assert!(!result.cancelled);
    let output = String::from_utf8(result.stdout).unwrap();
    let pid_line = output
        .lines()
        .find_map(|line| line.strip_prefix("partial:"))
        .expect("partial output contains child pids");
    let (direct, middle) = pid_line.split_once(':').unwrap();
    let grandchild = fs::read_to_string(root.path().join("tmp/grandchild.pid")).unwrap();
    for value in [direct, middle, grandchild.trim()] {
        assert_process_gone(value.parse().unwrap()).await;
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_capture_is_bounded_and_retains_head_and_tail() {
    let root = workspace("tag-team-macos-capture-");
    fs::write(root.path().join("repo/capture-a"), vec![b'A'; 180_000])
        .expect("write deterministic A capture fixture");
    fs::write(root.path().join("repo/capture-b"), vec![b'B'; 180_000])
        .expect("write deterministic B capture fixture");
    let sandbox = profile(root.path(), "cat capture-a; cat capture-b");
    let (_cancel, token) = CancellationToken::new();
    let result = spawn_confined(sandbox, token)
        .await
        .expect("bounded output command runs");
    assert_eq!(result.exit_code, Some(0));
    assert!(result.truncated);
    assert_eq!(result.stdout.len(), CAPTURE_LIMIT);
    assert!(
        result
            .stdout
            .iter()
            .take(CAPTURE_LIMIT / 2)
            .all(|byte| *byte == b'A')
    );
    assert!(
        result
            .stdout
            .iter()
            .skip(CAPTURE_LIMIT / 2)
            .all(|byte| *byte == b'B')
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_cancellation_kills_the_process_group() {
    let root = workspace("tag-team-macos-cancel-");
    let sandbox = profile(
        root.path(),
        "sleep 60 & child=$!; printf 'started:%s\\n' \"$child\"; wait",
    );
    let (cancel, token) = CancellationToken::new();
    let task = tokio::spawn(spawn_confined(sandbox, token));
    tokio::time::sleep(Duration::from_millis(250)).await;
    cancel.cancel();
    let result = task
        .await
        .unwrap()
        .expect("cancelled command returns an outcome");
    assert!(result.cancelled);
    assert!(!result.timed_out);
    let output = String::from_utf8(result.stdout).unwrap();
    let child = output
        .lines()
        .find_map(|line| line.strip_prefix("started:"))
        .expect("partial output contains the child pid");
    assert_process_gone(child.parse().unwrap()).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_process_watcher_kills_a_small_bounded_tree_at_the_pid_limit() {
    let root = workspace("tag-team-macos-pid-limit-");
    let (execution, mut resources) = test_limits();
    resources.processes = NonZeroU32::new(2).unwrap();
    let sandbox = SandboxProfile::new(
        root.path().to_path_buf(),
        "sleep 60 & sleep 60 & sleep 60 & wait".to_owned(),
        proxy_endpoint(root.path()),
        execution,
        resources,
    )
    .unwrap();
    let (_cancel, token) = CancellationToken::new();
    let result = spawn_confined(sandbox, token)
        .await
        .expect("process-limited command returns an outcome");
    assert!(
        result.resource_limit.is_some(),
        "the process tree must exceed the configured process limit"
    );
    assert!(!result.timed_out);
}

#[cfg(target_os = "macos")]
async fn assert_process_gone(pid: u32) {
    let nix_pid = Pid::from_raw(i32::try_from(pid).unwrap());
    for _ in 0..30 {
        match kill(nix_pid, None) {
            Err(Errno::ESRCH) => return,
            Err(error) => panic!("could not inspect child process: {error}"),
            Ok(()) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("process {pid} survived process-group termination");
}
