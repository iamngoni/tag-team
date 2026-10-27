//! Shared domain types and identifiers for durable Tag Team state.
//!
//! This module owns canonical wire and journal values; callers validate external
//! input before constructing state transitions.

use std::{
    fmt,
    path::PathBuf,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Maximum size of one repository or result bundle transferred over node RPC.
pub const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

macro_rules! ulid_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Ulid::generate())
            }

            #[must_use]
            pub fn as_str(&self) -> String {
                self.0.to_string()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = ulid::DecodeError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self(Ulid::from_str(value)?))
            }
        }
    };
}

ulid_id!(NodeId);
ulid_id!(SubtaskId);
ulid_id!(DispatchId);
ulid_id!(QuestionId);
ulid_id!(DeliveryId);
ulid_id!(CommandId);
ulid_id!(MainSessionId);

/// Milliseconds since the Unix epoch. SQLite stores this as a 64-bit integer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    #[must_use]
    pub fn now() -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self(i64::try_from(millis).unwrap_or(i64::MAX))
    }

    #[must_use]
    pub fn checked_add(self, duration: Duration) -> Self {
        let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
        Self(self.0.saturating_add(millis))
    }
}

/// A byte count that always serializes as an integer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct Bytes(pub u64);

impl Bytes {
    pub const KIB: u64 = 1_024;
    pub const MIB: u64 = 1_048_576;
    pub const GIB: u64 = 1_073_741_824;

    #[must_use]
    pub const fn kib(value: u64) -> Self {
        Self(value * Self::KIB)
    }

    #[must_use]
    pub const fn mib(value: u64) -> Self {
        Self(value * Self::MIB)
    }

    #[must_use]
    pub const fn gib(value: u64) -> Self {
        Self(value * Self::GIB)
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 >= Self::GIB {
            write!(f, "{:.1} GiB", self.0 as f64 / Self::GIB as f64)
        } else if self.0 >= Self::MIB {
            write!(f, "{:.1} MiB", self.0 as f64 / Self::MIB as f64)
        } else if self.0 >= Self::KIB {
            write!(f, "{:.1} KiB", self.0 as f64 / Self::KIB as f64)
        } else {
            write!(f, "{} B", self.0)
        }
    }
}

pub mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        serializer.serialize_u64(millis)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitOid(pub String);

impl GitOid {
    #[must_use]
    pub fn zero() -> Self {
        Self("0".repeat(40))
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoId(pub GitOid);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    #[serde(with = "hex_bytes")]
    pub sha256: [u8; 32],
    pub size: u64,
}

impl BlobRef {
    #[must_use]
    pub fn digest_hex(&self) -> String {
        self.sha256.iter().map(|b| format!("{b:02x}")).collect()
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        serializer.serialize_str(&hex)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex = String::deserialize(deserializer)?;
        if hex.len() != 64 {
            return Err(D::Error::custom("sha256 must be 64 hexadecimal characters"));
        }
        let mut out = [0_u8; 32];
        for (index, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let text = std::str::from_utf8(chunk).map_err(D::Error::custom)?;
            out[index] = u8::from_str_radix(text, 16).map_err(D::Error::custom)?;
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    MacOs,
    Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    Arm64,
    X64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
    pub os_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Enrolled,
    Active,
    OwnerBusy,
    Paused,
    Draining,
    Lost,
    Removed,
}

impl NodeState {
    #[must_use]
    pub const fn accepts_work(self) -> bool {
        matches!(self, Self::Active)
    }

    #[must_use]
    pub const fn is_removed(self) -> bool {
        matches!(self, Self::Removed)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    #[default]
    Own,
    Friend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thermal {
    Nominal,
    Fair,
    Serious,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capacity {
    pub cores_total: u16,
    pub cores_free: f32,
    pub memory_free: Bytes,
    pub disk_free: Bytes,
    pub running: u16,
    pub on_battery: bool,
    /// False when the host could not provide a trustworthy power-source sample.
    #[serde(default)]
    pub battery_known: bool,
    /// Minute-of-day from the node's local timezone; `None` fails closed for
    /// policies that declare allowed hours.
    #[serde(default)]
    pub local_minute_of_day: Option<u16>,
    pub thermal: Thermal,
}

impl Default for Capacity {
    fn default() -> Self {
        Self {
            cores_total: 1,
            cores_free: 1.0,
            memory_free: Bytes::gib(1),
            disk_free: Bytes::gib(1),
            running: 0,
            on_battery: false,
            battery_known: false,
            local_minute_of_day: None,
            thermal: Thermal::Nominal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toolchain {
    pub name: String,
    pub version: Version,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedRepo {
    pub repo: RepoId,
    pub heads: Vec<GitOid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnReturn {
    FinishRunning,
    Pause,
    HandBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OwnerPresence {
    Away {
        #[serde(with = "duration_ms")]
        idle_for: Duration,
    },
    Present,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HourRange {
    pub start_minutes: u16,
    pub end_minutes: u16,
}

impl HourRange {
    #[must_use]
    pub const fn contains(&self, minute_of_day: u16) -> bool {
        if self.start_minutes <= self.end_minutes {
            minute_of_day >= self.start_minutes && minute_of_day < self.end_minutes
        } else {
            minute_of_day >= self.start_minutes || minute_of_day < self.end_minutes
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodePolicy {
    pub only_when_idle: bool,
    #[serde(with = "duration_ms")]
    pub idle_after: Duration,
    pub on_owner_return: OnReturn,
    pub max_cores: Option<u16>,
    pub max_memory: Option<Bytes>,
    pub max_subtasks: u16,
    pub hours: Option<Vec<HourRange>>,
    pub never_on_battery: bool,
    pub extra_hosts: Vec<String>,
}

impl Default for NodePolicy {
    fn default() -> Self {
        Self {
            only_when_idle: false,
            idle_after: Duration::from_secs(10 * 60),
            on_owner_return: OnReturn::Pause,
            max_cores: None,
            max_memory: None,
            max_subtasks: 1,
            hours: None,
            never_on_battery: true,
            extra_hosts: Vec::new(),
        }
    }
}

impl NodePolicy {
    #[must_use]
    pub fn allows_now(&self, presence: OwnerPresence, minute_of_day: Option<u16>) -> bool {
        if self.only_when_idle && matches!(presence, OwnerPresence::Present) {
            return false;
        }
        if let Some(hours) = &self.hours {
            let Some(minute_of_day) = minute_of_day else {
                return false;
            };
            if !hours.iter().any(|range| range.contains(minute_of_day)) {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub name: String,
    pub platform: Platform,
    pub state: NodeState,
    #[serde(with = "hex_bytes")]
    pub cert_sha256: [u8; 32],
    pub capacity: Capacity,
    pub toolchains: Vec<Toolchain>,
    pub repo_heads: Vec<CachedRepo>,
    pub policy: NodePolicy,
    pub presence: OwnerPresence,
    pub last_seen: Timestamp,
    #[serde(default)]
    pub trust: Trust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReassignReason {
    NodeLost,
    OwnerReturned,
    NodePaused,
    NodeRemoved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum InterruptCause {
    NodeLost,
    NodeRemoved,
    HubRestarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum FailReason {
    TurnLimit,
    TimeLimit,
    ToolchainMissing(String),
    PatchTooLarge { limit: Bytes, actual: Bytes },
    Interrupted { cause: InterruptCause },
    Sandbox(String),
    Runner(String),
    Bundle(String),
    Model(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Needs {
    pub toolchains: Vec<(String, VersionReq)>,
    pub os: Option<Os>,
    pub arch: Option<Arch>,
    pub cores: u16,
    pub memory: Bytes,
    pub disk: Bytes,
}

impl Default for Needs {
    fn default() -> Self {
        Self {
            toolchains: Vec::new(),
            os: None,
            arch: None,
            cores: 1,
            memory: Bytes::gib(2),
            disk: Bytes::gib(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_turns: u32,
    #[serde(with = "duration_ms")]
    pub wall_clock: Duration,
    #[serde(with = "duration_ms")]
    pub command_timeout: Duration,
    pub max_output: Bytes,
    pub model_output: Bytes,
    pub max_patch: Bytes,
    #[serde(with = "duration_ms")]
    pub question_deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: 60,
            wall_clock: Duration::from_secs(45 * 60),
            command_timeout: Duration::from_secs(10 * 60),
            max_output: Bytes::kib(256),
            model_output: Bytes::kib(30),
            max_patch: Bytes::mib(8),
            question_deadline: Duration::from_secs(10 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskSpec {
    pub title: String,
    pub prompt: String,
    pub done_when: Vec<String>,
    pub repo: PathBuf,
    pub base_ref: String,
    pub include_uncommitted: bool,
    pub needs: Needs,
    pub limits: Limits,
    pub egress: Vec<String>,
    pub node_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Partial,
    NoChange,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assumption {
    pub what: String,
    pub why: String,
    pub from_timeout: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub what: String,
    pub command: Option<String>,
    pub passed: bool,
    pub output_tail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiffStat {
    pub files: u32,
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSummary {
    pub outcome: Outcome,
    pub summary: String,
    pub bundle: Option<BlobRef>,
    pub branch: Option<String>,
    pub diffstat: DiffStat,
    pub assumptions: Vec<Assumption>,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub turns: u32,
    pub commands: u32,
}

impl Usage {
    pub fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.turns = self.turns.saturating_add(other.turns);
        self.commands = self.commands.saturating_add(other.commands);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubtaskState {
    Queued,
    Placing,
    Preparing,
    Running,
    WaitingOnMain { question: QuestionId },
    Reassigning { reason: ReassignReason },
    Collecting,
    Done,
    Failed { reason: FailReason },
    Cancelled,
}

impl SubtaskState {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. } | Self::Cancelled)
    }

    /// Whether this state holds a runner/workspace slot for the account.
    ///
    /// `Placing` is intentionally excluded: it has not acquired a node yet, and
    /// treating an interrupted transition as active would permanently consume
    /// capacity until startup recovery repairs it.
    #[must_use]
    pub const fn occupies_worker_slot(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Running | Self::WaitingOnMain { .. } | Self::Collecting
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    pub id: DispatchId,
    pub main_session: MainSessionId,
    pub claude_session: Option<String>,
    pub repo: PathBuf,
    pub created: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtask {
    pub id: SubtaskId,
    pub dispatch: DispatchId,
    pub spec: SubtaskSpec,
    pub state: SubtaskState,
    pub node: Option<NodeId>,
    pub attempt: u16,
    pub base: GitOid,
    pub repo_id: RepoId,
    pub branch: String,
    pub runner_session: Option<String>,
    pub created: Timestamp,
    pub started: Option<Timestamp>,
    pub finished: Option<Timestamp>,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: QuestionId,
    pub subtask: SubtaskId,
    pub text: String,
    pub options: Vec<String>,
    pub context: Option<String>,
    pub deadline: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum AnsweredBy {
    MainClaude,
    You,
    Timeout { guess: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    Placed {
        node: NodeId,
    },
    Progress {
        note: String,
        percent: Option<u8>,
    },
    ToolRan {
        tool: String,
        summary: String,
        ok: bool,
        #[serde(with = "duration_ms")]
        took: Duration,
    },
    Question(Question),
    Answered {
        question: QuestionId,
        by: AnsweredBy,
    },
    Steered {
        text: String,
    },
    Reassigned {
        from: NodeId,
        reason: ReassignReason,
    },
    ReassignedTo {
        to: NodeId,
    },
    Finished(ResultSummary),
    Failed {
        reason: FailReason,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub subtask: SubtaskId,
    pub at: Timestamp,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeReport {
    pub state: NodeState,
    pub presence: OwnerPresence,
    pub capacity: Capacity,
    pub running: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_ref_round_trips_hex() {
        let blob = BlobRef {
            sha256: [7; 32],
            size: 42,
        };
        let encoded = serde_json::to_string(&blob).expect("serialize");
        let decoded: BlobRef = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(blob, decoded);
    }

    #[test]
    fn wrapped_hour_range_contains_after_midnight() {
        let range = HourRange {
            start_minutes: 22 * 60,
            end_minutes: 7 * 60,
        };
        assert!(range.contains(23 * 60));
        assert!(range.contains(6 * 60));
        assert!(!range.contains(12 * 60));
    }

    #[test]
    fn duration_uses_milliseconds_on_the_wire() {
        let limits = Limits::default();
        let encoded = serde_json::to_value(&limits).expect("serialize");
        assert_eq!(encoded["wall_clock"], 45 * 60 * 1000);
        let decoded: Limits = serde_json::from_value(encoded).expect("deserialize");
        assert_eq!(decoded, limits);
    }
}
