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

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tagteam_core::MAX_BLOB_BYTES;

/// The encoded helper envelope can exceed the blob limit by base64 and JSON overhead.
const MAX_HELPER_REQUEST_BYTES: u64 = MAX_BLOB_BYTES * 2;

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
    command.envs(request.env);
    let mut child = command
        .spawn()
        .map_err(|_| "command could not be started")?;
    let input = STANDARD
        .decode(request.stdin)
        .map_err(|_| "invalid command input")?;
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
        FsRequest::Collect { base } => {
            let bundle_path = root.join(".tagteam-output.bundle");
            run_git(
                &root,
                &[
                    "bundle",
                    "create",
                    bundle_path.to_str().ok_or("invalid bundle path")?,
                    "--all",
                ],
            )?;
            let bytes = fs::read(&bundle_path).map_err(|_| "git bundle could not be read")?;
            let _ = fs::remove_file(bundle_path);
            json!({"base": base, "data": STANDARD.encode(bytes)})
        }
    };
    Ok(result)
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
    fn rejects_parent_traversal() {
        let root = tempfile::tempdir().expect("root directory");
        let canonical = root.path().canonicalize().expect("canonical root");
        assert!(safe_path(&canonical, "../outside.txt").is_err());
        assert!(safe_path(&canonical, "/etc/passwd").is_err());
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
