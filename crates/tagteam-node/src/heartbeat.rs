//! Best-effort host measurements and capability probes for node heartbeats.
//!
//! Toolchain probes are cached for the process lifetime and failures are represented as missing
//! capability data. Owner absence is only reported when the platform supplies a valid idle-time
//! sample.

use std::{path::PathBuf, process::Command, sync::OnceLock};

use sysinfo::{Disks, System};
use tagteam_core::NodePolicy;

use crate::HeartbeatSource;

const TOOL_NAMES: &[&str] = &["cargo", "rustc", "node", "pnpm", "git", "python3"];
static TOOLCHAINS: OnceLock<Vec<tagteam_proto::v1::Toolchain>> = OnceLock::new();

#[derive(Default)]
pub struct HostHeartbeat {
    policy: Option<NodePolicy>,
}

impl HostHeartbeat {
    /// Create a heartbeat source using the node's validated scheduling policy.
    pub fn with_policy(policy: NodePolicy) -> Self {
        Self {
            policy: Some(policy),
        }
    }
}

impl HeartbeatSource for HostHeartbeat {
    fn heartbeat(&self) -> tagteam_proto::v1::Heartbeat {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        let cores = system.cpus().len();
        // Load average is available immediately without waiting for a second CPU sample.
        let load = System::load_average().one as f32;
        let disks = Disks::new_with_refreshed_list();
        let free_disk = disks
            .list()
            .iter()
            .map(|disk| disk.available_space())
            .max()
            .unwrap_or(0);
        let (presence, idle_for_ms) = owner_presence(self.policy.as_ref());
        let on_battery = detect_on_battery().unwrap_or(false);
        tagteam_proto::v1::Heartbeat {
            capacity: Some(tagteam_proto::v1::Capacity {
                cores_total: u32::try_from(cores).unwrap_or(u32::MAX),
                cores_free: (cores as f32 - load).clamp(0.0, cores as f32),
                memory_free: system.available_memory(),
                disk_free: free_disk,
                // CommandRunner does not expose active worker state to the heartbeat source.
                // Keep the wire value explicitly zero until a shared runner status API exists.
                running: 0,
                on_battery,
                // The protocol rejects an empty thermal state; nominal is the fallback when no
                // platform thermal sensor is available.
                thermal: "nominal".into(),
            }),
            presence: presence as i32,
            toolchains: TOOLCHAINS.get_or_init(probe_toolchains).clone(),
            repos: Vec::new(),
            running: Vec::new(),
            idle_for_ms,
        }
    }
}

fn owner_presence(policy: Option<&NodePolicy>) -> (tagteam_proto::v1::OwnerPresence, u64) {
    owner_presence_from_idle(policy, detect_idle_ms())
}

fn owner_presence_from_idle(
    policy: Option<&NodePolicy>,
    idle: Option<u64>,
) -> (tagteam_proto::v1::OwnerPresence, u64) {
    let Some(idle_ms) = idle else {
        return (tagteam_proto::v1::OwnerPresence::Present, 0);
    };
    let threshold_ms = policy
        .map(|policy| policy.idle_after.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(600_000);
    let presence = if idle_ms >= threshold_ms {
        tagteam_proto::v1::OwnerPresence::Away
    } else {
        tagteam_proto::v1::OwnerPresence::Present
    };
    (presence, idle_ms)
}

#[cfg(target_os = "macos")]
fn detect_idle_ms() -> Option<u64> {
    let output = Command::new("ioreg")
        .args(["-c", "IOHIDSystem", "-d", "4"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let nanoseconds = text.lines().find_map(|line| {
        let (_, value) = line.split_once("HIDIdleTime")?;
        value
            .split(|character: char| !character.is_ascii_digit())
            .find(|part| !part.is_empty())?
            .parse::<u64>()
            .ok()
    })?;
    Some(nanoseconds / 1_000_000)
}

#[cfg(not(target_os = "macos"))]
fn detect_idle_ms() -> Option<u64> {
    None
}

fn probe_toolchains() -> Vec<tagteam_proto::v1::Toolchain> {
    TOOL_NAMES
        .iter()
        .filter_map(|name| probe_toolchain(name))
        .collect()
}

fn probe_toolchain(name: &str) -> Option<tagteam_proto::v1::Toolchain> {
    let path = find_executable(name)?;
    let output = Command::new(&path).args(version_args(name)).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let version = parse_tool_version(name, &stdout)?;
    Some(tagteam_proto::v1::Toolchain {
        name: name.to_owned(),
        version,
        path: path.to_string_lossy().into_owned(),
    })
}

fn version_args(name: &str) -> &'static [&'static str] {
    match name {
        "rustc" => &["--version"],
        "python3" => &["--version"],
        _ => &["--version"],
    }
}

/// Parse a tool's first semantic version, tolerating common leading `v` and labels.
fn parse_tool_version(name: &str, output: &str) -> Option<String> {
    let token = output.split_whitespace().find(|token| {
        token
            .trim_start_matches('v')
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    })?;
    let token = token.trim_start_matches('v').trim_end_matches(',');
    let normalized = if token.matches('.').count() == 1 {
        format!("{token}.0")
    } else {
        token.to_owned()
    };
    semver::Version::parse(&normalized).ok().map(|version| {
        // The name is intentionally part of the parser contract for deterministic tests and future
        // tool-specific normalization, although current tools share semver's accepted format.
        let _ = name;
        version.to_string()
    })
}

fn find_executable(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

#[cfg(target_os = "macos")]
fn detect_on_battery() -> Option<bool> {
    let output = Command::new("pmset").args(["-g", "batt"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.contains("Battery Power"))
}

#[cfg(target_os = "linux")]
fn detect_on_battery() -> Option<bool> {
    let root = std::path::Path::new("/sys/class/power_supply");
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if std::fs::read_to_string(path.join("type")).ok()?.trim() == "Battery" {
            return Some(
                std::fs::read_to_string(path.join("status")).ok()?.trim() == "Discharging",
            );
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect_on_battery() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::{owner_presence_from_idle, parse_tool_version};
    use tagteam_core::NodePolicy;

    #[test]
    fn version_parser_accepts_common_tool_prefixes_and_rejects_garbage() {
        assert_eq!(
            parse_tool_version("node", "v22.4.1\n").as_deref(),
            Some("22.4.1")
        );
        assert_eq!(
            parse_tool_version("python3", "Python 3.12\n").as_deref(),
            Some("3.12.0")
        );
        assert_eq!(
            parse_tool_version("git", "git version 2.45.1").as_deref(),
            Some("2.45.1")
        );
        assert!(parse_tool_version("node", "not installed").is_none());
    }

    #[test]
    fn missing_idle_evidence_fails_safe_to_present() {
        assert_eq!(
            owner_presence_from_idle(None, None),
            (tagteam_proto::v1::OwnerPresence::Present, 0)
        );
    }

    #[test]
    fn only_when_idle_uses_the_configured_idle_threshold() {
        let policy = NodePolicy {
            only_when_idle: true,
            idle_after: std::time::Duration::from_secs(90),
            ..NodePolicy::default()
        };
        assert_eq!(
            owner_presence_from_idle(Some(&policy), Some(89_999)).0,
            tagteam_proto::v1::OwnerPresence::Present
        );
        assert_eq!(
            owner_presence_from_idle(Some(&policy), Some(90_000)).0,
            tagteam_proto::v1::OwnerPresence::Away
        );
    }

    #[test]
    fn default_presence_requires_the_default_ten_minute_idle_period() {
        assert_eq!(
            owner_presence_from_idle(None, Some(599_999)).0,
            tagteam_proto::v1::OwnerPresence::Present
        );
        assert_eq!(
            owner_presence_from_idle(None, Some(600_000)).0,
            tagteam_proto::v1::OwnerPresence::Away
        );
    }
}
