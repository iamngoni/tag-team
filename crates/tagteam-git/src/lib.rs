//! Safe Git CLI operations used by Tag Team hub and node flows.
//!
//! Every mutating operation is explicit. Snapshot creation uses a private index,
//! and bundle fetching/worktree management never checks out a destination branch.

use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use tagteam_core::{DiffStat, GitOid, RepoId};
use thiserror::Error;

pub const SECRET_DENYLIST: &[&str] = &[".env", ".env.*", "*.pem", "*.key", "id_*"];
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Git operation {operation} failed in {repo}: {message}")]
    Command {
        operation: &'static str,
        repo: PathBuf,
        message: String,
    },
    #[error("could not access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid Git reference or path: {0}")]
    InvalidInput(String),
    #[error("worktree destination already exists: {0}")]
    WorktreeCollision(PathBuf),
    #[error("result head {head} is not a descendant of result base {base}")]
    NonDescendant { base: String, head: String },
}

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Debug, Clone)]
pub struct Repository {
    path: PathBuf,
}

impl Repository {
    /// Open a repository or a directory within one, resolving its Git top level.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let out = run(
            path,
            "locate repository",
            [OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
        )?;
        let root = PathBuf::from(trim_line(&out.stdout));
        Ok(Self { path: root })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create an immutable snapshot commit from the selected base and current
    /// tracked/untracked state. The caller's index and worktree are never changed.
    pub fn snapshot(&self, base: &str) -> Result<Snapshot> {
        let base = self.resolve_commit(base)?;
        let index_path = temp_file_path("index")?;
        let _cleanup = RemoveFile(index_path.clone());
        let env = vec![(
            OsString::from("GIT_INDEX_FILE"),
            index_path.as_os_str().to_owned(),
        )];
        self.command_with_env(
            "initialize snapshot index",
            ["-c", "core.fsmonitor=false", "read-tree", &base],
            &env,
        )?;
        let paths = self.list_candidate_paths()?;
        let excluded: Vec<String> = paths
            .iter()
            .filter(|path| is_secret_path(path))
            .cloned()
            .collect();
        let mut add_args = vec![
            OsString::from("add"),
            OsString::from("-A"),
            OsString::from("--"),
            OsString::from("."),
        ];
        for path in &excluded {
            add_args.push(OsString::from(format!(":(exclude,literal){path}")));
        }
        add_args.splice(
            0..0,
            [OsString::from("-c"), OsString::from("core.fsmonitor=false")],
        );
        self.command_with_env("stage snapshot changes", add_args, &env)?;
        let tree = trim_line(
            &self
                .command_with_env("write snapshot tree", ["write-tree"], &env)?
                .stdout,
        );
        let identity_env = [
            (
                OsString::from("GIT_AUTHOR_NAME"),
                OsString::from("Tag Team"),
            ),
            (
                OsString::from("GIT_AUTHOR_EMAIL"),
                OsString::from("tagteam@invalid"),
            ),
            (
                OsString::from("GIT_COMMITTER_NAME"),
                OsString::from("Tag Team"),
            ),
            (
                OsString::from("GIT_COMMITTER_EMAIL"),
                OsString::from("tagteam@invalid"),
            ),
            (
                OsString::from("GIT_AUTHOR_DATE"),
                OsString::from("@0 +0000"),
            ),
            (
                OsString::from("GIT_COMMITTER_DATE"),
                OsString::from("@0 +0000"),
            ),
        ];
        let snapshot = trim_line(
            &self
                .command_with_env(
                    "create snapshot commit",
                    [
                        "-c",
                        "core.hooksPath=/dev/null",
                        "-c",
                        "core.fsmonitor=false",
                        "commit-tree",
                        &tree,
                        "-p",
                        &base,
                        "-m",
                        "Tag Team snapshot",
                    ],
                    &identity_env,
                )?
                .stdout,
        );
        Ok(Snapshot {
            base: GitOid(base),
            commit: GitOid(snapshot),
            excluded_paths: excluded,
        })
    }

    /// The lexicographically smallest object ID among reachable root commits.
    pub fn repo_id(&self, base: &str) -> Result<RepoId> {
        let base = self.resolve_commit(base)?;
        let output = self.command(["rev-list", "--max-parents=0", &base])?;
        let mut roots: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        roots.sort_unstable();
        roots
            .into_iter()
            .next()
            .map(|oid| RepoId(GitOid(oid)))
            .ok_or_else(|| {
                GitError::InvalidInput(format!("no root commit is reachable from {base}"))
            })
    }

    /// Create a full bundle or an incremental bundle with a valid advertised
    /// prerequisite. Invalid, unavailable, or unrelated prerequisites use full.
    pub fn create_bundle(
        &self,
        commit: &str,
        path: impl AsRef<Path>,
        advertised_heads: &[String],
        requested_prerequisite: Option<&str>,
    ) -> Result<Bundle> {
        let commit = self.resolve_commit(commit)?;
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| GitError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let prerequisite = requested_prerequisite.and_then(|requested| {
            advertised_heads
                .iter()
                .find(|head| head.as_str() == requested)
                .and_then(|_| {
                    self.resolve_commit(requested)
                        .ok()
                        .filter(|resolved| self.is_ancestor(resolved, &commit).unwrap_or(false))
                })
        });
        let reference = format!(
            "refs/tagteam/export/{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        self.command_named(
            "prepare bundle ref",
            ["update-ref", reference.as_str(), commit.as_str()],
        )?;
        let _ref_cleanup = RemoveRef {
            repo: self.path.clone(),
            reference: reference.clone(),
        };
        let mut args = vec![
            OsString::from("bundle"),
            OsString::from("create"),
            path.as_os_str().to_owned(),
            OsString::from(&reference),
        ];
        if let Some(parent) = &prerequisite {
            args.push(OsString::from(format!("^{parent}")));
        }
        self.command_named("create bundle", args)?;
        let metadata = fs::metadata(&path).map_err(|source| GitError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Bundle {
            path,
            commit: GitOid(commit),
            reference,
            prerequisite: prerequisite.map(GitOid),
            bytes: metadata.len(),
        })
    }

    /// Verify bundle object connectivity and prerequisite availability.
    pub fn verify_bundle(&self, bundle: impl AsRef<Path>) -> Result<()> {
        let bundle = bundle
            .as_ref()
            .canonicalize()
            .map_err(|source| GitError::Io {
                path: bundle.as_ref().to_path_buf(),
                source,
            })?;
        self.command_named(
            "verify bundle",
            [
                OsString::from("bundle"),
                OsString::from("verify"),
                bundle.as_os_str().to_owned(),
            ],
        )?;

        // Import into an isolated object database with the source repository as
        // a read-only alternate, so incremental prerequisites are available.
        let scratch = temp_file_path("bundle-fsck")?;
        fs::create_dir_all(&scratch).map_err(|source| GitError::Io {
            path: scratch.clone(),
            source,
        })?;
        let _scratch_cleanup = RemoveDir(scratch.clone());
        run(
            &scratch,
            "initialize bundle verification repository",
            ["init", "--bare", "-q"],
        )?;
        let objects = self.git_path("objects")?;
        let alternates = scratch.join("objects/info/alternates");
        fs::write(&alternates, format!("{}\n", objects.display())).map_err(|source| {
            GitError::Io {
                path: alternates.clone(),
                source,
            }
        })?;
        let heads = run(
            &self.path,
            "list bundle heads",
            [
                OsString::from("bundle"),
                OsString::from("list-heads"),
                bundle.as_os_str().to_owned(),
            ],
        )?;
        for (index, line) in String::from_utf8_lossy(&heads.stdout).lines().enumerate() {
            let Some(reference) = line.split_whitespace().nth(1) else {
                continue;
            };
            let destination = format!("refs/tagteam/fsck/{index}");
            run(
                &scratch,
                "fsck-checked bundle import",
                [
                    OsString::from("-c"),
                    OsString::from("transfer.fsckObjects=true"),
                    OsString::from("fetch"),
                    OsString::from("--no-tags"),
                    bundle.as_os_str().to_owned(),
                    OsString::from(format!("+{reference}:{destination}")),
                ],
            )?;
        }
        run(
            &scratch,
            "fsck bundle objects",
            ["fsck", "--full", "--strict", "--no-reflogs"],
        )?;
        Ok(())
    }

    /// Fetch a named branch from a bundle into this repository with object
    /// checking enabled. This does not check out or move any local branch.
    pub fn fetch_bundle(
        &self,
        bundle: impl AsRef<Path>,
        result_branch: &str,
        destination_ref: &str,
    ) -> Result<GitOid> {
        validate_ref_component(destination_ref)?;
        let bundle = bundle
            .as_ref()
            .canonicalize()
            .map_err(|source| GitError::Io {
                path: bundle.as_ref().to_path_buf(),
                source,
            })?;
        let source_ref = if result_branch.starts_with("refs/") {
            result_branch.to_owned()
        } else {
            format!("refs/heads/{result_branch}")
        };
        let destination = format!("refs/tagteam/import/{destination_ref}");
        self.validate_git_ref(&source_ref)?;
        self.validate_git_ref(&destination)?;
        let spec = format!("{source_ref}:{destination}");
        self.command_named(
            "fsck-checked bundle fetch",
            [
                OsString::from("-c"),
                OsString::from("transfer.fsckObjects=true"),
                OsString::from("fetch"),
                OsString::from("--no-tags"),
                bundle.into_os_string(),
                OsString::from(spec),
            ],
        )?;
        self.resolve_commit(&destination).map(GitOid)
    }

    /// Ensure `base` is an ancestor of `head` in this repository.
    pub fn check_ancestry(&self, base: &str, head: &str) -> Result<()> {
        let base = self.resolve_commit(base)?;
        let head = self.resolve_commit(head)?;
        if self.is_ancestor(&base, &head)? {
            Ok(())
        } else {
            Err(GitError::NonDescendant { base, head })
        }
    }

    /// Add a detached review worktree at `.tagteam/wt/<id>` and register the
    /// parent directory in this repository's private exclude file.
    pub fn add_review_worktree(&self, id: &str, commit: &str) -> Result<PathBuf> {
        validate_component(id)?;
        let commit = self.resolve_commit(commit)?;
        let destination = self.path.join(".tagteam").join("wt").join(id);
        if destination.exists() {
            return Err(GitError::WorktreeCollision(destination));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| GitError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let exclude = self.git_path("info/exclude")?;
        let ignore = ".tagteam/wt/\n";
        let existing = fs::read_to_string(&exclude).unwrap_or_default();
        if !existing.lines().any(|line| line.trim() == ignore.trim()) {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&exclude)
                .map_err(|source| GitError::Io {
                    path: exclude.clone(),
                    source,
                })?;
            if !existing.is_empty() && !existing.ends_with('\n') {
                file.write_all(b"\n").map_err(|source| GitError::Io {
                    path: exclude.clone(),
                    source,
                })?;
            }
            file.write_all(ignore.as_bytes())
                .map_err(|source| GitError::Io {
                    path: exclude.clone(),
                    source,
                })?;
        }
        self.command_named(
            "add detached review worktree",
            [
                OsString::from("-c"),
                OsString::from("core.hooksPath=/dev/null"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                OsString::from("--no-checkout"),
                destination.as_os_str().to_owned(),
                OsString::from(&commit),
            ],
        )?;
        if let Err(error) = self.command_named(
            "populate review worktree",
            [
                OsString::from("-C"),
                destination.as_os_str().to_owned(),
                OsString::from("-c"),
                OsString::from("core.hooksPath=/dev/null"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("checkout"),
                OsString::from("--detach"),
                OsString::from(&commit),
            ],
        ) {
            let _ = self.cleanup_review_worktree(id);
            return Err(error);
        }
        Ok(destination)
    }

    /// Remove only a registered worktree whose canonical path is under the
    /// repository's `.tagteam/wt` directory.
    pub fn cleanup_review_worktree(&self, id: &str) -> Result<()> {
        validate_component(id)?;
        let target = self.path.join(".tagteam").join("wt").join(id);
        if !target.exists() {
            return Ok(());
        }
        let canonical = target.canonicalize().map_err(|source| GitError::Io {
            path: target.clone(),
            source,
        })?;
        let parent = self
            .path
            .join(".tagteam")
            .join("wt")
            .canonicalize()
            .map_err(|source| GitError::Io {
                path: self.path.join(".tagteam/wt"),
                source,
            })?;
        if !canonical.starts_with(parent) {
            return Err(GitError::InvalidInput(
                "worktree path escapes .tagteam/wt".into(),
            ));
        }
        self.command_named(
            "remove review worktree",
            [
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                canonical.into_os_string(),
            ],
        )?;
        Ok(())
    }

    /// Diff summary between two commits, suitable for a landed branch report.
    pub fn diffstat(&self, base: &str, head: &str) -> Result<DiffStat> {
        let base = self.resolve_commit(base)?;
        let head = self.resolve_commit(head)?;
        let output = self.command(["diff", "--numstat", "-z", &base, &head, "--"])?;
        let mut files = 0_u32;
        let mut insertions = 0_u32;
        let mut deletions = 0_u32;
        let rows: Vec<_> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|field| !field.is_empty())
            .collect();
        for row in rows {
            let columns: Vec<_> = row.splitn(3, |b| *b == b'\t').collect();
            if columns.len() != 3 {
                continue;
            }
            files = files.saturating_add(1);
            insertions = insertions.saturating_add(
                std::str::from_utf8(columns[0])
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            );
            deletions = deletions.saturating_add(
                std::str::from_utf8(columns[1])
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            );
        }
        Ok(DiffStat {
            files,
            insertions,
            deletions,
        })
    }

    fn resolve_commit(&self, revision: &str) -> Result<String> {
        let output = self.command([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ])?;
        Ok(trim_line(&output.stdout))
    }

    fn is_ancestor(&self, base: &str, head: &str) -> Result<bool> {
        let output = Command::new("git")
            .args([
                OsStr::new("merge-base"),
                OsStr::new("--is-ancestor"),
                OsStr::new(base),
                OsStr::new(head),
            ])
            .current_dir(&self.path)
            .output()
            .map_err(|source| GitError::Io {
                path: self.path.clone(),
                source,
            })?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(command_error("check commit ancestry", &self.path, output)),
        }
    }

    fn validate_git_ref(&self, reference: &str) -> Result<()> {
        let output = Command::new("git")
            .args([OsStr::new("check-ref-format"), OsStr::new(reference)])
            .current_dir(&self.path)
            .output()
            .map_err(|source| GitError::Io {
                path: self.path.clone(),
                source,
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(GitError::InvalidInput(format!(
                "invalid Git ref {reference:?}"
            )))
        }
    }

    fn list_candidate_paths(&self) -> Result<Vec<String>> {
        let output = self.command([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])?;
        let mut paths: Vec<String> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    fn git_path(&self, path: &str) -> Result<PathBuf> {
        let output = self.command(["rev-parse", "--git-path", path])?;
        let path = PathBuf::from(trim_line(&output.stdout));
        Ok(if path.is_absolute() {
            path
        } else {
            self.path.join(path)
        })
    }

    fn command<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run(&self.path, "Git command", args)
    }

    fn command_named<I, S>(&self, operation: &'static str, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run(&self.path, operation, args)
    }

    fn command_with_env<I, S>(
        &self,
        operation: &'static str,
        args: I,
        env: &[(OsString, OsString)],
    ) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_env(&self.path, operation, args, env)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub base: GitOid,
    pub commit: GitOid,
    pub excluded_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub path: PathBuf,
    pub commit: GitOid,
    /// Fully qualified ref advertised inside the bundle.
    pub reference: String,
    pub prerequisite: Option<GitOid>,
    pub bytes: u64,
}

fn is_secret_path(path: &str) -> bool {
    path.split('/').any(|part| {
        part == ".env"
            || part.starts_with(".env.")
            || part.ends_with(".pem")
            || part.ends_with(".key")
            || part.starts_with("id_")
    })
}

fn validate_ref_component(value: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || value.contains(['\n', '\r', ':', ' ']) {
        return Err(GitError::InvalidInput(format!(
            "invalid ref component {value:?}"
        )));
    }
    Ok(())
}

fn validate_component(value: &str) -> Result<()> {
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\', '\0']) {
        return Err(GitError::InvalidInput(format!(
            "invalid worktree id {value:?}"
        )));
    }
    Ok(())
}

fn run<I, S>(repo: &Path, operation: &'static str, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_env(repo, operation, args, &[])
}

fn run_env<I, S>(
    repo: &Path,
    operation: &'static str,
    args: I,
    env: &[(OsString, OsString)],
) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(repo)
        .envs(env.iter().cloned());
    let output = command.output().map_err(|source| GitError::Io {
        path: repo.to_path_buf(),
        source,
    })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_error(operation, repo, output))
    }
}

fn command_error(operation: &'static str, repo: &Path, output: Output) -> GitError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    GitError::Command {
        operation,
        repo: repo.to_path_buf(),
        message: if stderr.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            stderr
        },
    }
}

fn trim_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn temp_file_path(label: &str) -> Result<PathBuf> {
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| GitError::InvalidInput(error.to_string()))?
        .as_nanos();
    let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(std::env::temp_dir().join(format!(
        "tagteam-{label}-{}-{tick}-{serial}",
        std::process::id()
    )))
}

struct RemoveFile(PathBuf);
impl Drop for RemoveFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct RemoveDir(PathBuf);
impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct RemoveRef {
    repo: PathBuf,
    reference: String,
}
impl Drop for RemoveRef {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .args([
                OsStr::new("update-ref"),
                OsStr::new("-d"),
                OsStr::new(&self.reference),
            ])
            .current_dir(&self.repo)
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct TempRepo(PathBuf);
    impl TempRepo {
        fn new(label: &str) -> Self {
            let path = temp_file_path(label).expect("temporary path");
            fs::create_dir_all(&path).expect("create repo directory");
            let repo = Self(path);
            repo.git(["init", "-q"]);
            repo.git(["config", "user.name", "Tag Team Tests"]);
            repo.git(["config", "user.email", "tagteam-tests@example.invalid"]);
            repo
        }
        fn git<I, S>(&self, args: I) -> Output
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }
        fn write(&self, name: &str, contents: &str) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().expect("file parent")).expect("create parents");
            fs::write(path, contents).expect("write file");
        }
        fn commit_all(&self, message: &str) -> String {
            self.git(["add", "-A"]);
            self.git(["commit", "-qm", message]);
            String::from_utf8_lossy(&self.git(["rev-parse", "HEAD"]).stdout)
                .trim()
                .to_owned()
        }
    }
    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snapshot_uses_private_index_excludes_secrets_and_has_stable_repo_id() {
        let fixture = TempRepo::new("git-snapshot");
        fixture.write("tracked.txt", "base\n");
        fixture.write(".env", "base secret\n");
        let base = fixture.commit_all("base");
        let repo = Repository::open(&fixture.0).expect("open repo");
        let root = repo.repo_id(&base).expect("repo id");
        assert_eq!(root, RepoId(GitOid(base.clone())));
        assert_eq!(repo.repo_id(&base).expect("repeat repo id"), root);
        fixture.git(["config", "--unset", "user.name"]);
        fixture.git(["config", "--unset", "user.email"]);

        fixture.write("tracked.txt", "changed\n");
        fixture.write(".env", "changed secret\n");
        fixture.write(".env.local", "secret\n");
        fixture.write("certs/private.pem", "private\n");
        fixture.write("id_node", "private\n");
        fixture.write("server.key", "private\n");
        fixture.write("new.txt", "new content\n");
        fs::remove_file(fixture.0.join("tracked.txt")).expect("delete tracked file");
        fixture.git(["add", "-N", "new.txt"]);
        let staged_index = fixture.git(["write-tree"]).stdout;
        let working_copy = fs::read(fixture.0.join(".env")).expect("read secret");
        let snapshot = repo.snapshot(&base).expect("snapshot");
        let repeated_snapshot = repo.snapshot(&base).expect("repeat snapshot");
        assert_eq!(repeated_snapshot.commit, snapshot.commit);
        assert_eq!(snapshot.base, GitOid(base.clone()));
        assert_eq!(
            snapshot.excluded_paths,
            [
                ".env",
                ".env.local",
                "certs/private.pem",
                "id_node",
                "server.key"
            ]
        );
        assert_eq!(fixture.git(["write-tree"]).stdout, staged_index);
        assert_eq!(
            fs::read(fixture.0.join(".env")).expect("secret unchanged"),
            working_copy
        );
        assert!(!fixture.0.join("tracked.txt").exists());
        let included = String::from_utf8_lossy(
            &fixture
                .git([
                    "diff-tree",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    &snapshot.commit.0,
                ])
                .stdout,
        )
        .to_string();
        assert!(included.contains("new.txt"));
        assert!(included.contains("tracked.txt"));
        assert!(!included.contains(".env"));
        assert!(!included.contains("private.pem"));
    }

    #[test]
    fn bundles_verify_fetch_and_enforce_ancestry_with_full_fallback() {
        let source = TempRepo::new("git-bundle-source");
        source.write("file.txt", "one\n");
        let first = source.commit_all("first");
        source.write("file.txt", "one\ntwo\n");
        let second = source.commit_all("second");
        let source_repo = Repository::open(&source.0).expect("source repo");
        let bundle_dir = temp_file_path("bundle-dir").expect("bundle path");
        fs::create_dir_all(&bundle_dir).expect("bundle directory");
        let full_path = bundle_dir.join("full.bundle");
        let full = source_repo
            .create_bundle(&second, &full_path, &[], None)
            .expect("full bundle");
        assert!(full.prerequisite.is_none());
        source_repo.verify_bundle(&full.path).expect("verify full");

        let incremental_path = bundle_dir.join("incremental.bundle");
        let incremental = source_repo
            .create_bundle(
                &second,
                &incremental_path,
                std::slice::from_ref(&first),
                Some(&first),
            )
            .expect("incremental bundle");
        assert_eq!(
            incremental.prerequisite.as_ref().map(|oid| oid.0.as_str()),
            Some(first.as_str())
        );
        assert!(incremental.bytes < full.bytes);
        source_repo
            .verify_bundle(&incremental.path)
            .expect("verify incremental");
        let fallback = source_repo
            .create_bundle(
                &second,
                bundle_dir.join("fallback.bundle"),
                &[],
                Some(&first),
            )
            .expect("full fallback");
        assert!(fallback.prerequisite.is_none());
        source_repo
            .verify_bundle(&fallback.path)
            .expect("verify fallback");

        let destination = TempRepo::new("git-bundle-destination");
        let destination_repo = Repository::open(&destination.0).expect("destination repo");
        let original_head = destination.git(["symbolic-ref", "--short", "HEAD"]).stdout;
        let fetched = destination_repo
            .fetch_bundle(&full.path, &full.reference, "run-1")
            .expect("fetch bundle");
        assert!(matches!(
            destination_repo.fetch_bundle(&full.path, &full.reference, "bad..ref"),
            Err(GitError::InvalidInput(_))
        ));
        destination_repo
            .check_ancestry(&first, &fetched.0)
            .expect("base ancestor");
        let other = TempRepo::new("git-unrelated");
        other.write("other.txt", "unrelated\n");
        let unrelated = other.commit_all("unrelated");
        destination.git([
            "fetch",
            &other.0.to_string_lossy(),
            "HEAD:refs/tagteam/unrelated",
        ]);
        assert_eq!(
            destination_repo
                .resolve_commit("refs/tagteam/unrelated")
                .unwrap(),
            unrelated
        );
        assert!(matches!(
            destination_repo.check_ancestry(&unrelated, &fetched.0),
            Err(GitError::NonDescendant { .. })
        ));
        assert_eq!(
            destination.git(["symbolic-ref", "--short", "HEAD"]).stdout,
            original_head
        );
        fs::remove_dir_all(bundle_dir).expect("remove bundle directory");
    }

    #[test]
    fn review_worktree_and_diffstat_are_safe_and_complete() {
        let fixture = TempRepo::new("git-worktree");
        fixture.write("a.txt", "old\nsecond old\n");
        let base = fixture.commit_all("base");
        fixture.write("a.txt", "new\n");
        fixture.write("b.txt", "added\nline two\n");
        let head = fixture.commit_all("landed");
        let repo = Repository::open(&fixture.0).expect("repo");
        let branch = fixture.git(["symbolic-ref", "--short", "HEAD"]).stdout;
        let worktree = repo
            .add_review_worktree("review-1", &head)
            .expect("add worktree");
        assert!(worktree.join("a.txt").exists());
        assert!(
            fs::read_to_string(repo.path().join(".git/info/exclude"))
                .expect("exclude")
                .contains(".tagteam/wt/")
        );
        assert!(matches!(
            repo.add_review_worktree("review-1", &head),
            Err(GitError::WorktreeCollision(_))
        ));
        repo.cleanup_review_worktree("review-1").expect("cleanup");
        assert!(!worktree.exists());
        assert_eq!(
            fixture.git(["symbolic-ref", "--short", "HEAD"]).stdout,
            branch
        );
        let stat = repo.diffstat(&base, &head).expect("diffstat");
        assert_eq!(
            stat,
            DiffStat {
                files: 2,
                insertions: 3,
                deletions: 2
            }
        );
    }
}
