//! Hub configuration and filesystem locations.
//!
//! The hub reads a small TOML file before opening the journal or binding any
//! socket. Paths are expanded and bounded here so later modules never interpret
//! raw user configuration.

use std::{
    collections::HashSet,
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use tagteam_core::Limits;
use thiserror::Error;

const DEFAULT_LISTEN: &str = "127.0.0.1:7443";
const DEFAULT_EGRESS: &[&str] = &[
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "proxy.golang.org",
];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration is invalid: {0}")]
    Invalid(String),
    #[error("configuration TOML is invalid: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Effective hub configuration after defaults, file loading, and validation.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub listen: SocketAddr,
    pub public_address: String,
    pub data_dir: PathBuf,
    pub max_concurrent_per_account: usize,
    pub runner_program: String,
    pub runner_args: Vec<String>,
    pub limits: Limits,
    pub heartbeat_timeout: Duration,
    /// Canonical baseline hosts allowed by workspace egress proxies.
    pub default_egress: Vec<String>,
    /// Ages after which terminal event history and unreferenced blobs are swept.
    pub retention: RetentionConfig,
}

/// Ages after which terminal events and unreferenced blobs become eligible for removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    pub events: Duration,
    pub blobs: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            events: Duration::from_secs(30 * 24 * 60 * 60),
            blobs: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HubFile {
    listen: Option<String>,
    public_address: Option<String>,
    data_dir: Option<PathBuf>,
    max_concurrent_per_account: Option<usize>,
    runner_program: Option<String>,
    runner_args: Option<Vec<String>>,
    default_egress: Option<Vec<String>>,
    limits: Option<LimitsFile>,
    retention: Option<RetentionFile>,
    heartbeat_timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsFile {
    max_turns: Option<u32>,
    wall_clock: Option<DurationInput>,
    wall_clock_seconds: Option<u64>,
    command_timeout: Option<DurationInput>,
    command_timeout_seconds: Option<u64>,
    max_output_bytes: Option<u64>,
    model_output_bytes: Option<u64>,
    max_patch_bytes: Option<u64>,
    question_deadline: Option<DurationInput>,
    question_deadline_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DurationInput {
    Seconds(u64),
    Human(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionFile {
    events: Option<DurationInput>,
    events_seconds: Option<u64>,
    blobs: Option<DurationInput>,
    blobs_seconds: Option<u64>,
}

impl HubConfig {
    /// Loads an explicit config file, or the default config when it exists.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(path) => Some(path.to_path_buf()),
            None => {
                let candidate = home_dir()?.join(".config/tagteam/hub.toml");
                candidate.exists().then_some(candidate)
            }
        };
        let raw = match path {
            Some(path) => {
                let text = fs::read_to_string(&path)?;
                toml::from_str::<HubFile>(&text)?
            }
            None => HubFile {
                listen: None,
                public_address: None,
                data_dir: None,
                max_concurrent_per_account: None,
                runner_program: None,
                runner_args: None,
                default_egress: None,
                limits: None,
                retention: None,
                heartbeat_timeout_seconds: None,
            },
        };
        Self::from_file(raw)
    }

    fn from_file(raw: HubFile) -> Result<Self, ConfigError> {
        let listen: SocketAddr = raw
            .listen
            .as_deref()
            .unwrap_or(DEFAULT_LISTEN)
            .parse()
            .map_err(|_| ConfigError::Invalid("listen must be a socket address".into()))?;
        let default_public_address = format!("https://{listen}");
        let public_address = raw.public_address.unwrap_or(default_public_address);
        tagteam_pki::Invite::new(
            public_address.clone(),
            tagteam_pki::CertificateFingerprint::from_certificate(b"configuration-validation"),
            [0; 32],
        )
        .map_err(|_| {
            ConfigError::Invalid("public_address must be a valid https:// host:port address".into())
        })?;
        let data_dir = expand_path(
            raw.data_dir
                .unwrap_or_else(|| PathBuf::from("~/.local/share/tagteam")),
        )?;
        let concurrency = raw.max_concurrent_per_account.unwrap_or(2);
        if concurrency == 0 || concurrency > 64 {
            return Err(ConfigError::Invalid(
                "max_concurrent_per_account must be between 1 and 64".into(),
            ));
        }
        let heartbeat_timeout = raw.heartbeat_timeout_seconds.unwrap_or(90);
        if !(15..=3_600).contains(&heartbeat_timeout) {
            return Err(ConfigError::Invalid(
                "heartbeat_timeout_seconds must be between 15 and 3600".into(),
            ));
        }
        let runner_program = raw.runner_program.unwrap_or_else(|| "node".into());
        if runner_program.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "runner_program must not be empty".into(),
            ));
        }
        let default_egress = normalize_and_dedupe_egress(
            raw.default_egress.unwrap_or_else(|| {
                DEFAULT_EGRESS
                    .iter()
                    .map(|host| (*host).to_owned())
                    .collect()
            }),
            "default_egress",
        )?;
        let mut limits = Limits::default();
        if let Some(file) = raw.limits {
            if let Some(value) = file.max_turns {
                if value == 0 {
                    return Err(ConfigError::Invalid("max_turns must be positive".into()));
                }
                limits.max_turns = value;
            }
            if let Some(value) =
                duration_override(file.wall_clock, file.wall_clock_seconds, "wall_clock")?
            {
                limits.wall_clock = value;
            }
            if let Some(value) = duration_override(
                file.command_timeout,
                file.command_timeout_seconds,
                "command_timeout",
            )? {
                limits.command_timeout = value;
            }
            if let Some(value) = file.max_output_bytes {
                limits.max_output = tagteam_core::Bytes(nonzero(value, "max_output_bytes")?);
            }
            if let Some(value) = file.model_output_bytes {
                limits.model_output = tagteam_core::Bytes(nonzero(value, "model_output_bytes")?);
            }
            if let Some(value) = file.max_patch_bytes {
                limits.max_patch = tagteam_core::Bytes(nonzero(value, "max_patch_bytes")?);
            }
            if let Some(value) = duration_override(
                file.question_deadline,
                file.question_deadline_seconds,
                "question_deadline",
            )? {
                limits.question_deadline = value;
            }
        }
        let mut retention = RetentionConfig::default();
        if let Some(file) = raw.retention {
            if let Some(value) =
                duration_override(file.events, file.events_seconds, "retention.events")?
            {
                retention.events = value;
            }
            if let Some(value) =
                duration_override(file.blobs, file.blobs_seconds, "retention.blobs")?
            {
                retention.blobs = value;
            }
        }
        Ok(Self {
            listen,
            public_address,
            data_dir,
            max_concurrent_per_account: concurrency,
            runner_program,
            runner_args: raw.runner_args.unwrap_or_default(),
            limits,
            heartbeat_timeout: Duration::from_secs(heartbeat_timeout),
            default_egress,
            retention,
        })
    }

    /// Creates the data directory and returns the stable file locations used by
    /// the hub, local API, journal, blobs, and CA material.
    pub fn prepare_data_dir(&self) -> Result<DataPaths, ConfigError> {
        fs::create_dir_all(&self.data_dir)?;
        set_private_directory(&self.data_dir)?;
        Ok(DataPaths {
            journal: self.data_dir.join("hub.sqlite3"),
            blobs: self.data_dir.join("blobs"),
            socket: self.data_dir.join("hub.sock"),
            ca_certificate: self.data_dir.join("ca.pem"),
            ca_private_key: self.data_dir.join("ca-key.pem"),
        })
    }
}

/// Stable paths inside the hub data directory.
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub journal: PathBuf,
    pub blobs: PathBuf,
    pub socket: PathBuf,
    pub ca_certificate: PathBuf,
    pub ca_private_key: PathBuf,
}

fn home_dir() -> Result<PathBuf, ConfigError> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ConfigError::Invalid("HOME is not set".into()))
}

fn expand_path(path: PathBuf) -> Result<PathBuf, ConfigError> {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    if text.starts_with('~') {
        return Err(ConfigError::Invalid(
            "only ~ and ~/ paths can be expanded".into(),
        ));
    }
    Ok(path)
}

fn nonzero(value: u64, field: &str) -> Result<u64, ConfigError> {
    if value == 0 {
        Err(ConfigError::Invalid(format!("{field} must be positive")))
    } else {
        Ok(value)
    }
}

fn nonzero_duration(seconds: u64, field: &str) -> Result<Duration, ConfigError> {
    nonzero(seconds, field).map(Duration::from_secs)
}

fn duration_override(
    friendly: Option<DurationInput>,
    legacy_seconds: Option<u64>,
    field: &str,
) -> Result<Option<Duration>, ConfigError> {
    match (friendly, legacy_seconds) {
        (Some(_), Some(_)) => Err(ConfigError::Invalid(format!(
            "specify either {field} or its legacy seconds field, not both"
        ))),
        (None, None) => Ok(None),
        (Some(input), None) => {
            let duration = match input {
                DurationInput::Seconds(value) => Duration::from_secs(value),
                DurationInput::Human(value) => tagteam_core::human::parse_duration(&value)
                    .map_err(|error| ConfigError::Invalid(format!("{field}: {error}")))?,
            };
            if duration.is_zero() {
                return Err(ConfigError::Invalid(format!("{field} must be positive")));
            }
            Ok(Some(duration))
        }
        (None, Some(seconds)) => Ok(Some(nonzero_duration(seconds, field)?)),
    }
}

pub(crate) fn normalize_and_dedupe_egress(
    hosts: Vec<String>,
    field: &str,
) -> Result<Vec<String>, ConfigError> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(hosts.len());
    for host in hosts {
        let host = normalize_egress_host(&host)
            .map_err(|reason| ConfigError::Invalid(format!("{field} contains {reason}")))?;
        if seen.insert(host.clone()) {
            normalized.push(host);
        }
    }
    Ok(normalized)
}

pub(crate) fn normalize_egress_host(host: &str) -> Result<String, &'static str> {
    let host = host.trim().strip_suffix('.').unwrap_or(host.trim());
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        return Err("an invalid hostname");
    }
    if host.parse::<IpAddr>().is_ok() {
        return Err("an IP address; exact DNS hostnames are required");
    }
    if !host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Err("an invalid exact DNS hostname");
    }
    Ok(host.to_ascii_lowercase())
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(text: &str) -> Result<HubConfig, ConfigError> {
        HubConfig::from_file(toml::from_str(text)?)
    }

    #[test]
    fn parses_friendly_hub_limits_retention_and_deduped_default_egress() {
        let config = parse_config(
            r#"
data_dir = "/tmp/tagteam-hub-config-test"
default_egress = ["CRATES.IO", "crates.io.", "git.example"]

[limits]
wall_clock = "45m"
command_timeout = "10m"
question_deadline = "10m"

[retention]
events = "30d"
blobs = "7d"
"#,
        )
        .expect("config parses");
        assert_eq!(config.limits.wall_clock, Duration::from_secs(45 * 60));
        assert_eq!(config.limits.command_timeout, Duration::from_secs(10 * 60));
        assert_eq!(
            config.limits.question_deadline,
            Duration::from_secs(10 * 60)
        );
        assert_eq!(config.retention.events, Duration::from_secs(30 * 86_400));
        assert_eq!(config.retention.blobs, Duration::from_secs(7 * 86_400));
        assert_eq!(config.default_egress, ["crates.io", "git.example"]);
    }

    #[test]
    fn legacy_seconds_fields_and_default_retention_remain_supported() {
        let config = parse_config(
            "data_dir='/tmp/tagteam-hub-config-test'\n[limits]\nwall_clock_seconds=90\ncommand_timeout_seconds=30\nquestion_deadline_seconds=60\n",
        )
        .expect("legacy config parses");
        assert_eq!(config.limits.wall_clock, Duration::from_secs(90));
        assert_eq!(config.limits.command_timeout, Duration::from_secs(30));
        assert_eq!(config.limits.question_deadline, Duration::from_secs(60));
        assert_eq!(config.retention, RetentionConfig::default());
    }

    #[test]
    fn invalid_or_conflicting_duration_fields_are_rejected() {
        for text in [
            "[limits]\nwall_clock='0m'",
            "[limits]\nwall_clock='1.5h'",
            "[limits]\nwall_clock='18446744073709551615w'",
            "[limits]\nwall_clock='45m'\nwall_clock_seconds=2700",
            "[retention]\nevents='0d'",
            "[retention]\nblobs='1.5d'",
        ] {
            assert!(parse_config(text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn default_egress_requires_exact_dns_hostnames() {
        for host in [
            "",
            "*.example.org",
            "https://example.org",
            "127.0.0.1",
            "bad..host",
        ] {
            let text = format!("default_egress=['{host}']");
            assert!(parse_config(&text).is_err(), "accepted host {host:?}");
        }
    }
}
