//! Hub configuration and filesystem locations.
//!
//! The hub reads a small TOML file before opening the journal or binding any
//! socket. Paths are expanded and bounded here so later modules never interpret
//! raw user configuration.

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use tagteam_core::Limits;
use thiserror::Error;

const DEFAULT_LISTEN: &str = "127.0.0.1:7443";

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
    limits: Option<LimitsFile>,
    heartbeat_timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsFile {
    max_turns: Option<u32>,
    wall_clock_seconds: Option<u64>,
    command_timeout_seconds: Option<u64>,
    max_output_bytes: Option<u64>,
    model_output_bytes: Option<u64>,
    max_patch_bytes: Option<u64>,
    question_deadline_seconds: Option<u64>,
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
                limits: None,
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
        let mut limits = Limits::default();
        if let Some(file) = raw.limits {
            if let Some(value) = file.max_turns {
                if value == 0 {
                    return Err(ConfigError::Invalid("max_turns must be positive".into()));
                }
                limits.max_turns = value;
            }
            if let Some(value) = file.wall_clock_seconds {
                limits.wall_clock = nonzero_duration(value, "wall_clock_seconds")?;
            }
            if let Some(value) = file.command_timeout_seconds {
                limits.command_timeout = nonzero_duration(value, "command_timeout_seconds")?;
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
            if let Some(value) = file.question_deadline_seconds {
                limits.question_deadline = nonzero_duration(value, "question_deadline_seconds")?;
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
