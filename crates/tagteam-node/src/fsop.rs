//! Sandboxed workspace filesystem and Git operations.
//!
//! The hidden helper runs only inside `spawn_confined`; every path is validated relative to
//! the canonical repository root, and symlinks are rejected before reads or writes.

use std::{
    fs,
    io::{Read, Seek, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(target_os = "linux")]
use std::{
    io,
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tagteam_core::MAX_BLOB_BYTES;

/// The encoded helper envelope can exceed the blob limit by base64 and JSON overhead.
const MAX_HELPER_REQUEST_BYTES: u64 = MAX_BLOB_BYTES * 2;
#[cfg(target_os = "linux")]
const PROXY_BRIDGE_PORT: u16 = 38_417;

const PROXY_ENV_NAMES: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FsRequest {
    Read {
        files: Vec<FileRead>,
    },
    Write {
        files: Vec<FileWrite>,
    },
    Edit {
        path: String,
        old: String,
        new: String,
        replace_all: bool,
    },
    Glob {
        pattern: String,
        cwd: String,
        limit: u32,
    },
    Grep {
        pattern: String,
        path: String,
        glob: String,
        ignore_case: bool,
        context: u32,
        limit: u32,
    },
    Prepare {
        bundle: String,
        base: String,
        branch: String,
    },
    Commit {
        message: String,
    },
    Collect {
        base: String,
        message: String,
    },
    Delete {
        path: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct FileRead {
    pub path: String,
    pub offset: u64,
    pub limit: u64,
}

#[derive(Serialize, Deserialize)]
pub struct FileWrite {
    pub path: String,
    pub data: String,
    pub expected_sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub stdin: String,
}

#[cfg(target_os = "linux")]
struct ProxyBridge {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl ProxyBridge {
    fn start(socket: PathBuf, address: SocketAddr) -> Result<Self, String> {
        if !address.ip().is_loopback() {
            return Err("sandbox proxy bridge address is invalid".into());
        }
        let listener = TcpListener::bind(address)
            .map_err(|_| "sandbox proxy bridge could not bind its loopback endpoint")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "sandbox proxy bridge could not configure its listener")?;
        let address = listener
            .local_addr()
            .map_err(|_| "sandbox proxy bridge address is unavailable")?;
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = Arc::clone(&stopping);
        let task = thread::Builder::new()
            .name("tagteam-proxy-bridge".into())
            .spawn(move || {
                while !thread_stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((client, _)) if !thread_stopping.load(Ordering::Acquire) => {
                            let socket = socket.clone();
                            let _ = thread::Builder::new()
                                .name("tagteam-proxy-bridge-client".into())
                                .spawn(move || relay_proxy_stream(client, socket));
                        }
                        Ok((client, _)) => drop(client),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|_| "sandbox proxy bridge could not start")?;
        Ok(Self {
            address,
            stopping,
            task: Some(task),
        })
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

#[cfg(target_os = "linux")]
impl Drop for ProxyBridge {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn relay_proxy_stream(mut client: TcpStream, socket: PathBuf) {
    let Ok(mut upstream) = UnixStream::connect(socket) else {
        return;
    };
    let Ok(mut upstream_reader) = upstream.try_clone() else {
        return;
    };
    let Ok(mut client_writer) = client.try_clone() else {
        return;
    };
    let writer = thread::spawn(move || {
        let _ = io::copy(&mut upstream_reader, &mut client_writer);
        let _ = client_writer.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut client, &mut upstream);
    let _ = upstream.shutdown(Shutdown::Write);
    let _ = writer.join();
}

#[cfg(not(target_os = "linux"))]
fn normalize_proxy_url(value: &str) -> Result<String, String> {
    let address = value
        .strip_prefix("http://")
        .and_then(|value| value.parse::<std::net::SocketAddr>().ok())
        .filter(|address| address.ip().is_loopback() && address.port() != 0)
        .ok_or("sandbox proxy environment is invalid")?;
    Ok(format!("http://{address}"))
}

fn trusted_proxy_url() -> Result<String, String> {
    let value = std::env::var("HTTP_PROXY").map_err(|_| "sandbox proxy environment is missing")?;
    #[cfg(target_os = "linux")]
    {
        if value != "http://unix:/run/proxy.sock" {
            return Err("sandbox Unix proxy environment is invalid".into());
        }
        Ok(format!("http://127.0.0.1:{PROXY_BRIDGE_PORT}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        normalize_proxy_url(&value)
    }
}

fn apply_proxy_environment(
    command: &mut Command,
    env: &std::collections::BTreeMap<String, String>,
    url: &str,
) {
    for (key, value) in env {
        if !PROXY_ENV_NAMES
            .iter()
            .any(|reserved| key.eq_ignore_ascii_case(reserved))
        {
            command.env(key, value);
        }
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(key, url);
    }
    for key in ["NO_PROXY", "no_proxy"] {
        command.env(key, "");
    }
}

/// Reads one bounded, base64-encoded command request from stdin and executes it.
pub fn run_exec_encoded_from_reader(reader: impl Read) -> Result<i32, String> {
    let encoded = read_encoded_request(reader)?;
    run_exec_encoded(&encoded)
}

pub fn run_exec_encoded(encoded: &str) -> Result<i32, String> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "invalid command request")?;
    let request: ExecRequest =
        serde_json::from_slice(&bytes).map_err(|_| "invalid command request")?;
    let root = repo_root()?;
    if request.argv.is_empty() {
        return Err("command argv must not be empty".into());
    }
    let cwd = if request.cwd.is_empty() {
        root.clone()
    } else {
        safe_path(&root, &request.cwd)?
    };
    if !cwd.is_dir() {
        return Err("command cwd is not a directory".into());
    }
    if request
        .env
        .keys()
        .any(|key| key.is_empty() || key.contains('=') || key.contains('\0'))
    {
        return Err("invalid command environment".into());
    }
    let input = STANDARD
        .decode(request.stdin)
        .map_err(|_| "invalid command input")?;
    let trusted_proxy_url = trusted_proxy_url()?;
    #[cfg(target_os = "linux")]
    let _proxy_bridge = {
        let socket = std::env::var_os("TAGTEAM_PROXY_SOCKET")
            .filter(|socket| socket == "/run/proxy.sock")
            .ok_or("sandbox proxy socket is unavailable")?;
        let address = trusted_proxy_url
            .strip_prefix("http://")
            .and_then(|value| value.parse::<SocketAddr>().ok())
            .ok_or("sandbox proxy environment is invalid")?;
        if address.port() != PROXY_BRIDGE_PORT {
            return Err("sandbox proxy bridge port does not match its policy".into());
        }
        let bridge = ProxyBridge::start(PathBuf::from(socket), address)?;
        if bridge.url() != trusted_proxy_url {
            return Err("sandbox proxy bridge endpoint does not match its policy".into());
        }
        bridge
    };
    let mut command = Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("TMPDIR", std::env::var("TMPDIR").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_proxy_environment(&mut command, &request.env, &trusted_proxy_url);
    let mut child = command
        .spawn()
        .map_err(|_| "command could not be started")?;
    let mut stdin = child.stdin.take().ok_or("command input unavailable")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let stdout = child.stdout.take().ok_or("command output unavailable")?;
    let stderr = child.stderr.take().ok_or("command output unavailable")?;
    let out_thread = std::thread::spawn(move || {
        std::io::copy(&mut stdout.take(usize::MAX as u64), &mut std::io::stdout())
    });
    let err_thread = std::thread::spawn(move || {
        std::io::copy(&mut stderr.take(usize::MAX as u64), &mut std::io::stderr())
    });
    let status = child.wait().map_err(|_| "command wait failed")?;
    let _ = writer.join();
    let _ = out_thread.join();
    let _ = err_thread.join();
    Ok(status.code().unwrap_or(1))
}

fn repo_root() -> Result<PathBuf, String> {
    let root = std::env::current_dir().map_err(|_| "workspace unavailable")?;
    root.canonicalize()
        .map_err(|_| "workspace unavailable".into())
}

fn safe_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || relative.is_empty()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("path must be a non-empty workspace-relative path".into());
    }
    let mut current = root.to_path_buf();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("workspace symlinks are not allowed".into());
            }
            Ok(_) => {
                let canonical = current
                    .canonicalize()
                    .map_err(|_| "workspace path is unavailable")?;
                if !canonical.starts_with(root) {
                    return Err("path escapes workspace".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("workspace path is unavailable".into()),
        }
    }
    Ok(current)
}

fn run_git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    // The supervisor injects the resolved macOS developer Git when the system
    // shim would otherwise need host-only Xcode state. Linux keeps PATH lookup.
    let executable = std::env::var_os("TAGTEAM_GIT_BIN").unwrap_or_else(|| "git".into());
    let output = Command::new(executable)
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_1", "false")
        .output()
        .map_err(|_| "git could not be started")?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(2048)
            .collect());
    }
    Ok(output.stdout)
}

fn walk(
    root: &Path,
    current: &Path,
    output: &mut Vec<PathBuf>,
    limit: usize,
) -> Result<(), String> {
    if output.len() >= limit {
        return Ok(());
    }
    let entries = fs::read_dir(current).map_err(|_| "workspace directory could not be read")?;
    for entry in entries {
        if output.len() >= limit {
            break;
        }
        let entry = entry.map_err(|_| "workspace entry could not be read")?;
        let file_type = entry
            .file_type()
            .map_err(|_| "workspace entry could not be inspected")?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk(root, &path, output, limit)?;
        } else if file_type.is_file()
            && path.canonicalize().is_ok_and(|path| path.starts_with(root))
        {
            output.push(path);
        }
    }
    Ok(())
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let (mut p, mut v, mut star, mut mark) = (0, 0, None, 0);
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = v;
        } else if let Some(index) = star {
            p = index + 1;
            mark += 1;
            v = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Reads one bounded, base64-encoded filesystem request from stdin and executes it.
pub fn run_encoded_from_reader(reader: impl Read) -> Result<String, String> {
    read_encoded_request(reader).map(|encoded| run_encoded(&encoded))
}

pub fn run_encoded(encoded: &str) -> String {
    let response = match run_request(encoded) {
        Ok(value) => serde_json::json!({"ok": true, "value": value}),
        Err(error) => serde_json::json!({"ok": false, "error": error}),
    };
    STANDARD.encode(serde_json::to_vec(&response).unwrap_or_default())
}

fn read_encoded_request(mut reader: impl Read) -> Result<String, String> {
    let mut encoded = String::new();
    reader
        .by_ref()
        .take(MAX_HELPER_REQUEST_BYTES + 1)
        .read_to_string(&mut encoded)
        .map_err(|_| "helper request could not be read")?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_HELPER_REQUEST_BYTES {
        return Err("helper request exceeds the allowed size".into());
    }
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return Err("helper request is empty".into());
    }
    Ok(encoded.to_owned())
}

fn run_request(encoded: &str) -> Result<Value, String> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "invalid helper request")?;
    let request: FsRequest =
        serde_json::from_slice(&bytes).map_err(|_| "invalid helper request")?;
    let root = repo_root()?;
    let result = match request {
        FsRequest::Read { files } => {
            let mut output = Vec::new();
            for file in files {
                let path = safe_path(&root, &file.path)?;
                let mut bytes = Vec::new();
                let mut handle =
                    fs::File::open(path).map_err(|_| "workspace file could not be read")?;
                handle
                    .seek(std::io::SeekFrom::Start(file.offset))
                    .map_err(|_| "workspace file could not be read")?;
                handle
                    .take(file.limit)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "workspace file could not be read")?;
                output.push(json!({"path": file.path, "data": STANDARD.encode(bytes), "offset": file.offset}));
            }
            json!({"files": output})
        }
        FsRequest::Write { files } => {
            let mut paths = Vec::new();
            for file in files {
                let path = safe_path(&root, &file.path)?;
                let parent = path.parent().ok_or("invalid workspace path")?;
                fs::create_dir_all(parent)
                    .map_err(|_| "workspace directory could not be created")?;
                if !parent
                    .canonicalize()
                    .map_err(|_| "workspace directory unavailable")?
                    .starts_with(&root)
                {
                    return Err("path escapes workspace".into());
                }
                let bytes = STANDARD
                    .decode(file.data)
                    .map_err(|_| "invalid file body")?;
                if let Some(expected) = file.expected_sha256 {
                    use sha2::Digest;
                    if data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&bytes)) != expected {
                        return Err("file digest did not match".into());
                    }
                }
                fs::write(path, bytes).map_err(|_| "workspace file could not be written")?;
                paths.push(file.path);
            }
            json!({"paths": paths})
        }
        FsRequest::Edit {
            path,
            old,
            new,
            replace_all,
        } => {
            let target = safe_path(&root, &path)?;
            let contents = fs::read_to_string(&target)
                .map_err(|_| "workspace file is not UTF-8 or could not be read")?;
            let count = if replace_all {
                contents.matches(&old).count()
            } else {
                usize::from(contents.contains(&old))
            };
            if count == 0 || old.is_empty() {
                return Err("edit target was not found".into());
            }
            let updated = if replace_all {
                contents.replace(&old, &new)
            } else {
                contents.replacen(&old, &new, 1)
            };
            fs::write(target, updated).map_err(|_| "workspace file could not be written")?;
            json!({"path": path, "replacements": count})
        }
        FsRequest::Glob {
            pattern,
            cwd,
            limit,
        } => {
            let start = if cwd.is_empty() {
                root.clone()
            } else {
                safe_path(&root, &cwd)?
            };
            let mut files = Vec::new();
            walk(
                &root,
                &start,
                &mut files,
                usize::try_from(limit.max(1)).unwrap_or(usize::MAX),
            )?;
            let matches = files
                .into_iter()
                .filter_map(|path| {
                    path.strip_prefix(&root)
                        .ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                })
                .filter(|path| wildcard(&pattern, path))
                .collect::<Vec<_>>();
            json!({"lines": matches, "truncated": false})
        }
        FsRequest::Grep {
            pattern,
            path,
            glob,
            ignore_case,
            context,
            limit,
        } => {
            let start = if path.is_empty() {
                root.clone()
            } else {
                safe_path(&root, &path)?
            };
            let mut files = Vec::new();
            if start.is_file() {
                files.push(start);
            } else {
                walk(&root, &start, &mut files, 100_000)?;
            }
            let needle = if ignore_case {
                pattern.to_lowercase()
            } else {
                pattern.clone()
            };
            let mut lines = Vec::new();
            for file in files {
                let name = file
                    .strip_prefix(&root)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !glob.is_empty() && !wildcard(&glob, &name) {
                    continue;
                }
                let Ok(contents) = fs::read_to_string(file) else {
                    continue;
                };
                let source_lines = contents.lines().collect::<Vec<_>>();
                let matching = source_lines
                    .iter()
                    .enumerate()
                    .filter_map(|(index, line)| {
                        let candidate = if ignore_case {
                            line.to_lowercase()
                        } else {
                            (*line).to_owned()
                        };
                        candidate.contains(&needle).then_some(index)
                    })
                    .collect::<Vec<_>>();
                let mut selected = std::collections::BTreeSet::new();
                for index in matching {
                    let radius = usize::try_from(context).unwrap_or(usize::MAX);
                    let start = index.saturating_sub(radius);
                    let end = index
                        .saturating_add(radius)
                        .min(source_lines.len().saturating_sub(1));
                    selected.extend(start..=end);
                }
                for index in selected {
                    let line = source_lines[index];
                    let candidate = if ignore_case {
                        line.to_lowercase()
                    } else {
                        line.to_owned()
                    };
                    if candidate.contains(&needle) {
                        lines.push(format!("{name}:{}:{line}", index + 1));
                    } else {
                        lines.push(format!("{name}:{}-{line}", index + 1));
                    }
                    if lines.len() >= usize::try_from(limit.max(1)).unwrap_or(usize::MAX) {
                        break;
                    }
                }
                if lines.len() >= usize::try_from(limit.max(1)).unwrap_or(usize::MAX) {
                    break;
                }
            }
            json!({"lines": lines, "truncated": false})
        }
        FsRequest::Prepare {
            bundle,
            base,
            branch,
        } => {
            if base.starts_with('-') || branch.starts_with('-') {
                return Err("invalid base or branch".into());
            }
            run_git(&root, &["check-ref-format", "--branch", &branch])?;
            let bytes = STANDARD
                .decode(bundle)
                .map_err(|_| "invalid bundle bytes")?;
            let bundle_path = root.join(".tagteam-input.bundle");
            fs::write(&bundle_path, bytes).map_err(|_| "bundle could not be staged")?;
            run_git(&root, &["init", "--initial-branch", &branch])?;
            run_git(
                &root,
                &[
                    "fetch",
                    bundle_path.to_str().ok_or("invalid bundle path")?,
                    &base,
                ],
            )?;
            run_git(&root, &["checkout", "-B", &branch, "FETCH_HEAD"])?;
            let _ = fs::remove_file(bundle_path);
            json!({"ok": true})
        }
        FsRequest::Commit { message } => {
            run_git(&root, &["add", "-A"])?;
            run_git(&root, &["commit", "-m", &message])?;
            let oid = String::from_utf8(run_git(&root, &["rev-parse", "HEAD"])?.to_vec())
                .map_err(|_| "git returned invalid commit id")?
                .trim()
                .to_owned();
            json!({"oid": oid})
        }
        FsRequest::Collect { base, message } => {
            let (path, size) = collect_workspace(&root, &message)?;
            json!({"base": base, "path": path, "size": size})
        }
        FsRequest::Delete { path } => {
            let target = safe_path(&root, &path)?;
            fs::remove_file(target).map_err(|_| "workspace file could not be removed")?;
            json!({"deleted": path})
        }
    };
    Ok(result)
}

fn collect_workspace(root: &Path, message: &str) -> Result<(String, u64), String> {
    if message.chars().count() > 512 {
        return Err("collect commit message exceeds 512 characters".into());
    }
    run_git(root, &["add", "-A"])?;
    run_git(
        root,
        &[
            "-c",
            "user.name=Tag Team Node",
            "-c",
            "user.email=tagteam-node@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            message,
        ],
    )?;
    let path = create_bundle(root)?;
    let size = fs::metadata(&path)
        .map_err(|_| "git bundle could not be inspected")?
        .len();
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "git bundle path is invalid")?
        .to_string_lossy()
        .into_owned();
    Ok((relative, size))
}

fn create_bundle(root: &Path) -> Result<PathBuf, String> {
    let bundle_path = root.join(".tagteam-output.bundle");
    run_git(
        root,
        &[
            "bundle",
            "create",
            bundle_path.to_str().ok_or("invalid bundle path")?,
            "--all",
        ],
    )?;
    Ok(bundle_path)
}

/// Encodes helper requests into the single argument accepted by the hidden CLI mode.
pub fn encode_request(request: &FsRequest) -> Result<String, String> {
    let bytes = serde_json::to_vec(request).map_err(|_| "helper request could not be encoded")?;
    Ok(STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_proxy_environment_wins_over_all_caller_proxy_values() {
        let caller = std::collections::BTreeMap::from([
            ("hTtP_PrOxY".into(), "http://outside.invalid:8080".into()),
            ("NO_PROXY".into(), "*".into()),
            ("no_proxy".into(), "localhost".into()),
            ("BUILD_FLAG".into(), "kept".into()),
        ]);
        let mut command = Command::new("true");
        command.env_clear();
        apply_proxy_environment(&mut command, &caller, "http://127.0.0.1:38417");
        let values = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert_eq!(
                values.get(key).map(String::as_str),
                Some("http://127.0.0.1:38417")
            );
        }
        assert_eq!(values.get("NO_PROXY").map(String::as_str), Some(""));
        assert_eq!(values.get("no_proxy").map(String::as_str), Some(""));
        assert_eq!(values.get("BUILD_FLAG").map(String::as_str), Some("kept"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn proxy_url_must_resolve_to_a_nonzero_loopback_endpoint() {
        assert_eq!(
            normalize_proxy_url("http://127.0.0.1:38417").expect("valid loopback proxy"),
            "http://127.0.0.1:38417"
        );
        for value in [
            "http://8.8.8.8:8080",
            "http://localhost:8080",
            "http://127.0.0.1:0",
            "http://user@127.0.0.1:8080",
            "http://127.0.0.1:8080/path",
        ] {
            assert!(normalize_proxy_url(value).is_err(), "{value}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn loopback_bridge_relays_to_unix_proxy_and_stops_cleanly() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().expect("temporary directory");
        let socket = directory.path().join("proxy.sock");
        let server = UnixListener::bind(&socket).expect("fixture socket binds");
        let server_task = thread::spawn(move || {
            let (mut stream, _) = server.accept().expect("bridge connects");
            let mut request = [0; 4];
            stream.read_exact(&mut request).expect("request arrives");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("response sends");
        });
        let bridge = ProxyBridge::start(
            socket,
            "127.0.0.1:0".parse().expect("loopback endpoint parses"),
        )
        .expect("bridge starts");
        let bridge_address = bridge.address;
        let mut client = TcpStream::connect(bridge.address).expect("TCP client connects");
        client.write_all(b"ping").expect("request sends");
        let mut response = [0; 4];
        client
            .read_exact(&mut response)
            .expect("proxy response arrives");
        assert_eq!(&response, b"pong");
        drop(client);
        drop(bridge);
        server_task.join().expect("fixture server exits");
        assert!(TcpListener::bind(bridge_address).is_ok());
    }

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git starts");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = tempfile::tempdir().expect("root directory");
        let canonical = root.path().canonicalize().expect("canonical root");
        assert!(safe_path(&canonical, "../outside.txt").is_err());
        assert!(safe_path(&canonical, "/etc/passwd").is_err());
    }

    #[test]
    fn collect_commits_workspace_changes_before_bundling() {
        let root = tempfile::tempdir().expect("root directory");
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("result.txt"), "collected result\n").expect("write result");

        let (bundle, size) =
            collect_workspace(root.path(), "collect result").expect("bundle created");
        assert!(size > 0);
        let head = String::from_utf8(git(root.path(), &["rev-parse", "HEAD"]))
            .expect("commit ID is UTF-8");
        let bundle_path = root.path().join(bundle);
        assert_eq!(
            String::from_utf8(git(root.path(), &["log", "-1", "--format=%s"]))
                .expect("commit subject is UTF-8")
                .trim(),
            "collect result"
        );
        let bundle_heads = git(
            root.path(),
            &[
                "bundle",
                "list-heads",
                bundle_path.to_str().expect("UTF-8 path"),
            ],
        );
        assert!(
            String::from_utf8(bundle_heads)
                .expect("bundle heads are UTF-8")
                .contains(head.trim())
        );
        assert!(
            bundle_path.is_file(),
            "the bounded bundle stays in the workspace for chunked upload"
        );
        assert_eq!(
            String::from_utf8(git(
                root.path(),
                &["status", "--porcelain", "--untracked-files=no"],
            ))
            .expect("status is UTF-8"),
            ""
        );
    }

    #[test]
    fn collect_creates_empty_commit_when_workspace_has_no_changes() {
        let root = tempfile::tempdir().expect("root directory");
        git(root.path(), &["init", "-q"]);

        let (bundle, _) = collect_workspace(root.path(), "collect empty result")
            .expect("empty workspace is committed and bundled");
        let bundle_path = root.path().join(bundle);
        let bundle_heads = git(
            root.path(),
            &[
                "bundle",
                "list-heads",
                bundle_path.to_str().expect("UTF-8 path"),
            ],
        );
        assert!(!bundle_heads.is_empty());
        assert_eq!(
            String::from_utf8(git(root.path(), &["log", "-1", "--format=%s"]))
                .expect("commit subject is UTF-8")
                .trim(),
            "collect empty result"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let root = tempfile::tempdir().expect("root directory");
        let outside = tempfile::tempdir().expect("outside directory");
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape"))
            .expect("symlink created");
        let canonical = root.path().canonicalize().expect("canonical root");
        assert!(safe_path(&canonical, "escape/secret.txt").is_err());
    }
}
