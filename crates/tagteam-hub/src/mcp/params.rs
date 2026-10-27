//! Typed MCP inputs and conversion into the durable core request types.
//!
//! MCP schemas are intentionally defined at this boundary. Core types remain
//! transport-neutral, and every optional field is resolved and validated before
//! a request can reach the daemon.

use std::{path::Path, str::FromStr, time::Duration};

use schemars::JsonSchema;
use semver::VersionReq;
use serde::Deserialize;
use tagteam_core::{
    Arch, Bytes, Limits, Needs, Os, QuestionId, SubtaskId, SubtaskSpec, local_api::AnswerAuthor,
};

/// Maximum long-poll duration accepted by the MCP inbox tool.
pub const MAX_INBOX_WAIT_MS: u64 = 50_000;

const DEFAULT_TAIL: u32 = 100;
const MAX_TAIL: u32 = 4_096;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchParams {
    /// Independent subtasks to place on enrolled nodes.
    pub subtasks: Vec<SubtaskParams>,
    /// Prefer one subtask per node instead of packing compatible work together.
    #[serde(default)]
    pub spread: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubtaskParams {
    /// Short human-readable title.
    pub title: String,
    /// Complete instructions for the remote agent.
    pub prompt: String,
    /// Commands that must exit successfully before the result is accepted.
    pub done_when: Vec<String>,
    /// Optional repository path. Omit it to use the MCP session's repository.
    #[serde(default)]
    pub repo: Option<std::path::PathBuf>,
    /// Git base revision. Defaults to HEAD.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Snapshot tracked and untracked working-tree changes. Defaults to true.
    #[serde(default)]
    pub include_uncommitted: Option<bool>,
    /// Node requirements used for placement.
    #[serde(default)]
    pub needs: Option<NeedsParams>,
    /// Per-subtask execution limits. Omit to use the hub defaults.
    #[serde(default)]
    pub limits: Option<LimitsParams>,
    /// Additional egress hostnames granted to the sandbox.
    #[serde(default)]
    pub egress: Vec<String>,
    /// Optional node name preference.
    #[serde(default)]
    pub node_hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NeedsParams {
    /// Required toolchains, for example `cargo` with requirement `>=1.80`.
    #[serde(default)]
    pub toolchains: Vec<ToolchainNeed>,
    /// Required operating system.
    #[serde(default)]
    pub os: Option<OsParam>,
    /// Required CPU architecture.
    #[serde(default)]
    pub arch: Option<ArchParam>,
    /// Minimum free CPU cores. Defaults to 1.
    #[serde(default)]
    pub cores: Option<u16>,
    /// Minimum free memory in MiB. Defaults to 2048.
    #[serde(default)]
    pub memory_mib: Option<u64>,
    /// Minimum free disk in MiB. Defaults to 1024.
    #[serde(default)]
    pub disk_mib: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolchainNeed {
    /// Executable or toolchain name.
    pub name: String,
    /// Semver requirement, for example `>=1.80` or `^22`.
    pub requirement: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OsParam {
    #[serde(rename = "macos", alias = "mac_os")]
    MacOs,
    Linux,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchParam {
    Arm64,
    X64,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LimitsParams {
    /// Maximum agent turns. Defaults to 60.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Wall-clock limit in milliseconds. Defaults to 45 minutes.
    #[serde(default)]
    pub wall_clock_ms: Option<u64>,
    /// Per-command timeout in milliseconds. Defaults to 10 minutes.
    #[serde(default)]
    pub command_timeout_ms: Option<u64>,
    /// Captured command output cap in KiB. Defaults to 256.
    #[serde(default)]
    pub max_output_kib: Option<u64>,
    /// Model-visible output cap in KiB. Defaults to 30.
    #[serde(default)]
    pub model_output_kib: Option<u64>,
    /// Maximum result patch size in MiB. Defaults to 8.
    #[serde(default)]
    pub max_patch_mib: Option<u64>,
    /// Time allowed for a main-session answer, in milliseconds. Defaults to 10 minutes.
    #[serde(default)]
    pub question_deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusParams {
    /// Optional subtask IDs. Omit to read all subtasks.
    #[serde(default)]
    pub ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxParams {
    /// Return events with sequence numbers greater than this cursor.
    #[serde(default)]
    pub since_seq: u64,
    /// Long-poll duration in milliseconds. Values above 50,000 are capped.
    #[serde(default)]
    pub wait_ms: u64,
    /// Optional event-kind filter such as `progress` or `failed`.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsParams {
    /// Subtask ID.
    pub subtask: String,
    /// Number of trailing rendered events to return. Defaults to 100.
    #[serde(default)]
    pub tail: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    /// Subtask ID.
    pub subtask: String,
    /// Human-readable cancellation reason.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectParams {
    /// Subtask ID.
    pub subtask: String,
    /// Create a review worktree at `.tagteam/wt/<id>` when true.
    #[serde(default)]
    pub worktree: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookBriefParams {
    /// Claude Code session ID supplied by the plugin hook.
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookStopParams {
    /// Claude Code session ID supplied by the plugin hook.
    pub session_id: String,
    /// Claude Code's recursion guard from the Stop hook payload.
    #[serde(default)]
    pub stop_hook_active: String,
}

impl HookStopParams {
    /// Whether the current Stop event was already caused by a stop hook.
    #[must_use]
    pub fn stop_hook_active(&self) -> bool {
        matches!(
            self.stop_hook_active.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerParams {
    /// Subtask ID.
    pub subtask: String,
    /// Answer text. This is treated as a quoted user/coordinator response.
    pub text: String,
    /// Optional open question ID. The daemon rejects a mismatch.
    #[serde(default)]
    pub question: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SteerParams {
    /// Subtask ID.
    pub subtask: String,
    /// New direction for the running agent.
    pub text: String,
}

impl SubtaskParams {
    /// Resolves defaults and validates one subtask before dispatch.
    pub fn into_spec(self, repo_root: &Path) -> Result<SubtaskSpec, String> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err("subtask title must not be empty".into());
        }
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            return Err(format!("subtask {title:?} prompt must not be empty"));
        }
        if self.done_when.is_empty() {
            return Err(format!(
                "subtask {title:?} needs at least one done_when command"
            ));
        }
        let mut done_when = Vec::with_capacity(self.done_when.len());
        for command in self.done_when {
            let command = command.trim();
            if command.is_empty() {
                return Err(format!("subtask {title:?} has an empty done_when command"));
            }
            done_when.push(command.to_owned());
        }

        let repo = match self.repo {
            Some(repo) => {
                if !repo.is_absolute() {
                    return Err("subtask repo must be an absolute path".into());
                }
                let repo = repo
                    .canonicalize()
                    .map_err(|error| format!("subtask repo is not accessible: {error}"))?;
                let root = repo_root
                    .canonicalize()
                    .map_err(|error| format!("session repository is not accessible: {error}"))?;
                if repo != root {
                    return Err(format!(
                        "subtask repo {} does not match the MCP session repository {}",
                        repo.display(),
                        root.display()
                    ));
                }
                root
            }
            None => repo_root.to_path_buf(),
        };

        let base_ref = self.base_ref.unwrap_or_else(|| "HEAD".into());
        if base_ref.trim().is_empty() {
            return Err("base_ref must not be empty".into());
        }
        let egress = self
            .egress
            .into_iter()
            .map(|host| host.trim().to_owned())
            .collect::<Vec<_>>();
        if egress.iter().any(String::is_empty) {
            return Err("egress hosts must not be empty".into());
        }

        Ok(SubtaskSpec {
            title: title.to_owned(),
            prompt: prompt.to_owned(),
            done_when,
            repo,
            base_ref: base_ref.trim().to_owned(),
            include_uncommitted: self.include_uncommitted.unwrap_or(true),
            needs: self
                .needs
                .map(NeedsParams::into_needs)
                .transpose()?
                .unwrap_or_default(),
            limits: self
                .limits
                .map(LimitsParams::into_limits)
                .transpose()?
                .unwrap_or_default(),
            egress,
            node_hint: self
                .node_hint
                .map(|hint| hint.trim().to_owned())
                .filter(|hint| !hint.is_empty()),
        })
    }
}

impl NeedsParams {
    fn into_needs(self) -> Result<Needs, String> {
        let defaults = Needs::default();
        let mut toolchains = Vec::with_capacity(self.toolchains.len());
        for toolchain in self.toolchains {
            let name = toolchain.name.trim();
            if name.is_empty() {
                return Err("toolchain name must not be empty".into());
            }
            let requirement = VersionReq::parse(toolchain.requirement.trim())
                .map_err(|error| format!("invalid requirement for {name}: {error}"))?;
            toolchains.push((name.to_owned(), requirement));
        }
        let cores = self.cores.unwrap_or(defaults.cores);
        if cores == 0 {
            return Err("needs.cores must be greater than zero".into());
        }
        Ok(Needs {
            toolchains,
            os: self.os.map(|os| match os {
                OsParam::MacOs => Os::MacOs,
                OsParam::Linux => Os::Linux,
            }),
            arch: self.arch.map(|arch| match arch {
                ArchParam::Arm64 => Arch::Arm64,
                ArchParam::X64 => Arch::X64,
            }),
            cores,
            memory: Bytes::mib(self.memory_mib.unwrap_or(defaults.memory.0 / Bytes::MIB)),
            disk: Bytes::mib(self.disk_mib.unwrap_or(defaults.disk.0 / Bytes::MIB)),
        })
    }
}

impl LimitsParams {
    fn into_limits(self) -> Result<Limits, String> {
        let defaults = Limits::default();
        let limits = Limits {
            max_turns: self.max_turns.unwrap_or(defaults.max_turns),
            wall_clock: duration_ms(self.wall_clock_ms, defaults.wall_clock, "wall_clock_ms")?,
            command_timeout: duration_ms(
                self.command_timeout_ms,
                defaults.command_timeout,
                "command_timeout_ms",
            )?,
            max_output: bytes_kib(self.max_output_kib, defaults.max_output, "max_output_kib")?,
            model_output: bytes_kib(
                self.model_output_kib,
                defaults.model_output,
                "model_output_kib",
            )?,
            max_patch: bytes_mib(self.max_patch_mib, defaults.max_patch, "max_patch_mib")?,
            question_deadline: duration_ms(
                self.question_deadline_ms,
                defaults.question_deadline,
                "question_deadline_ms",
            )?,
        };
        if limits.max_turns == 0 {
            return Err("limits.max_turns must be greater than zero".into());
        }
        Ok(limits)
    }
}

fn duration_ms(value: Option<u64>, default: Duration, field: &str) -> Result<Duration, String> {
    let duration = value.map(Duration::from_millis).unwrap_or(default);
    if duration.is_zero() {
        return Err(format!("limits.{field} must be greater than zero"));
    }
    Ok(duration)
}

fn bytes_kib(value: Option<u64>, default: Bytes, field: &str) -> Result<Bytes, String> {
    let bytes = value.map(Bytes::kib).unwrap_or(default);
    if bytes.0 == 0 {
        return Err(format!("limits.{field} must be greater than zero"));
    }
    Ok(bytes)
}

fn bytes_mib(value: Option<u64>, default: Bytes, field: &str) -> Result<Bytes, String> {
    let bytes = value.map(Bytes::mib).unwrap_or(default);
    if bytes.0 == 0 {
        return Err(format!("limits.{field} must be greater than zero"));
    }
    Ok(bytes)
}

pub fn parse_subtask(value: &str) -> Result<SubtaskId, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("subtask ID must not be empty".into());
    }
    SubtaskId::from_str(value).map_err(|error| format!("invalid subtask ID {value:?}: {error}"))
}

pub fn parse_subtasks(values: Option<Vec<String>>) -> Result<Option<Vec<SubtaskId>>, String> {
    let Some(values) = values else {
        return Ok(None);
    };
    if values.is_empty() {
        return Ok(None);
    }
    values
        .into_iter()
        .map(|value| parse_subtask(&value))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub fn parse_question(value: Option<String>) -> Result<Option<QuestionId>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Err("question ID must not be empty".into());
    }
    QuestionId::from_str(value)
        .map(Some)
        .map_err(|error| format!("invalid question ID {value:?}: {error}"))
}

pub fn answer_author() -> AnswerAuthor {
    AnswerAuthor::MainClaude
}

pub fn capped_wait_ms(wait_ms: u64) -> u64 {
    wait_ms.min(MAX_INBOX_WAIT_MS)
}

pub fn tail_or_default(tail: Option<u32>) -> u32 {
    tail.unwrap_or(DEFAULT_TAIL).min(MAX_TAIL)
}

pub fn cancel_reason(reason: Option<String>) -> Result<String, String> {
    let reason = reason.unwrap_or_else(|| "cancelled by user".into());
    let reason = reason.trim();
    if reason.is_empty() {
        return Err("cancel reason must not be empty".into());
    }
    Ok(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SubtaskParams {
        SubtaskParams {
            title: "Fix the parser".into(),
            prompt: "Repair the parser and add a regression test.".into(),
            done_when: vec!["cargo test -p parser".into()],
            repo: None,
            base_ref: None,
            include_uncommitted: None,
            needs: None,
            limits: None,
            egress: Vec::new(),
            node_hint: None,
        }
    }

    #[test]
    fn subtask_defaults_use_the_session_repository() {
        let repo = tempfile::tempdir().expect("repo");
        let spec = params().into_spec(repo.path()).expect("spec");
        assert_eq!(spec.repo, repo.path());
        assert_eq!(spec.base_ref, "HEAD");
        assert!(spec.include_uncommitted);
        assert_eq!(spec.limits, Limits::default());
        assert_eq!(spec.needs, Needs::default());
    }

    #[test]
    fn dispatch_rejects_ambiguous_or_unbounded_inputs() {
        let repo = tempfile::tempdir().expect("repo");
        let mut empty_checks = params();
        empty_checks.done_when.clear();
        assert!(empty_checks.into_spec(repo.path()).is_err());

        let mut zero_turns = params();
        zero_turns.limits = Some(LimitsParams {
            max_turns: Some(0),
            ..LimitsParams::default()
        });
        assert!(zero_turns.into_spec(repo.path()).is_err());

        let mut bad_requirement = params();
        bad_requirement.needs = Some(NeedsParams {
            toolchains: vec![ToolchainNeed {
                name: "cargo".into(),
                requirement: "not a requirement".into(),
            }],
            os: None,
            arch: None,
            cores: None,
            memory_mib: None,
            disk_mib: None,
        });
        assert!(bad_requirement.into_spec(repo.path()).is_err());
    }

    #[test]
    fn inbox_wait_and_log_tail_are_capped() {
        assert_eq!(capped_wait_ms(10), 10);
        assert_eq!(capped_wait_ms(u64::MAX), MAX_INBOX_WAIT_MS);
        assert_eq!(tail_or_default(None), DEFAULT_TAIL);
        assert_eq!(tail_or_default(Some(u32::MAX)), MAX_TAIL);
    }
}
