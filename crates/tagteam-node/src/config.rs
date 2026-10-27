//! Typed node configuration loaded before identity or session startup.
//!
//! Config values are validated once at the CLI boundary; the optional invite is only read by
//! `join` and is cleared when the effective configuration is persisted after enrollment.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Deserializer, Serialize};
use tagteam_core::{Bytes, HourRange, NodePolicy, OnReturn};

const DEFAULT_CONFIG: &str = ".config/tagteam/node.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("node config could not be read or written: {0}")]
    Io(#[from] std::io::Error),
    #[error("node config TOML is invalid: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("node config could not be serialized")]
    Serialize(#[from] toml::ser::Error),
    #[error("node config is invalid: {0}")]
    Invalid(String),
}

/// Effective user and scheduling config for one node installation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// One-time enrollment invite; consumed by `join` and omitted from persisted config.
    #[serde(default, skip_serializing)]
    pub hub_invite: Option<String>,
    #[serde(default)]
    pub only_when_idle: bool,
    /// Idle threshold in seconds after accepting either an integer or friendly TOML duration.
    #[serde(
        default = "default_idle_after",
        deserialize_with = "deserialize_idle_after"
    )]
    pub idle_after: u64,
    #[serde(default = "default_on_owner_return")]
    pub on_owner_return: OnReturn,
    pub max_cores: Option<u32>,
    /// Memory limit in bytes after accepting either an integer or friendly TOML byte size.
    #[serde(default, deserialize_with = "deserialize_optional_bytes")]
    pub max_memory: Option<u64>,
    #[serde(default = "default_max_subtasks")]
    pub max_subtasks: u32,
    #[serde(default)]
    pub hours: Option<Vec<HourRange>>,
    #[serde(default = "default_true")]
    pub never_on_battery: bool,
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// Root commit IDs this node may execute. `None` preserves the trusted
    /// own-machine default; `Some([])` denies every repository and is the
    /// correct starting point for a friend's machine.
    #[serde(default)]
    pub allowed_repo_ids: Option<Vec<String>>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: default_name(),
            data_dir: default_data_dir(),
            hub_invite: None,
            only_when_idle: false,
            idle_after: default_idle_after(),
            on_owner_return: OnReturn::Pause,
            max_cores: None,
            max_memory: None,
            max_subtasks: default_max_subtasks(),
            hours: None,
            never_on_battery: true,
            extra_hosts: Vec::new(),
            allowed_repo_ids: None,
        }
    }
}

impl NodeConfig {
    /// Resolve an explicit path or the conventional per-user config, using defaults if absent.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let selected = match path {
            Some(path) => Some(path.to_path_buf()),
            None => default_config_path()?.filter(|candidate| candidate.exists()),
        };
        let mut config = if let Some(path) = selected {
            toml::from_str(&fs::read_to_string(path)?)?
        } else {
            Self::default()
        };
        config.validate()?;
        config.data_dir = expand_path(&config.data_dir)?;
        Ok(config)
    }

    /// Persist effective non-secret settings, creating a private config directory on Unix.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        let serialized = toml::to_string_pretty(self)?;
        fs::write(path, serialized)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Convert the validated user settings to the hub's domain policy.
    pub fn policy(&self) -> NodePolicy {
        NodePolicy {
            only_when_idle: self.only_when_idle,
            idle_after: Duration::from_secs(self.idle_after),
            on_owner_return: self.on_owner_return,
            max_cores: self.max_cores.and_then(|value| u16::try_from(value).ok()),
            max_memory: self.max_memory.map(Bytes),
            max_subtasks: u16::try_from(self.max_subtasks).unwrap_or(u16::MAX),
            hours: self.hours.clone(),
            never_on_battery: self.never_on_battery,
            extra_hosts: self.extra_hosts.clone(),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.trim().is_empty() || self.name.len() > 128 {
            return Err(ConfigError::Invalid(
                "name must contain 1 to 128 bytes".into(),
            ));
        }
        if self.idle_after > 24 * 60 * 60 {
            return Err(ConfigError::Invalid(
                "idle_after must be at most 86400 seconds".into(),
            ));
        }
        if self.max_cores == Some(0)
            || self
                .max_cores
                .is_some_and(|value| value > u32::from(u16::MAX))
        {
            return Err(ConfigError::Invalid(
                "max_cores must be between 1 and 65535".into(),
            ));
        }
        if self.max_memory == Some(0) {
            return Err(ConfigError::Invalid(
                "max_memory must be greater than zero".into(),
            ));
        }
        if self.max_subtasks == 0 || self.max_subtasks > u32::from(u16::MAX) {
            return Err(ConfigError::Invalid(
                "max_subtasks must be between 1 and 65535".into(),
            ));
        }
        if self.hours.as_ref().is_some_and(|hours| {
            hours
                .iter()
                .any(|range| range.start_minutes > 1439 || range.end_minutes > 1439)
        }) {
            return Err(ConfigError::Invalid(
                "hours must use minute-of-day values from 0 through 1439".into(),
            ));
        }
        if self
            .extra_hosts
            .iter()
            .any(|host| host.trim().is_empty() || host.len() > 253)
        {
            return Err(ConfigError::Invalid(
                "extra_hosts entries must contain 1 to 253 bytes".into(),
            ));
        }
        if let Some(repositories) = &self.allowed_repo_ids
            && repositories.iter().any(|repository| {
                repository.len() != 40 || !repository.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(ConfigError::Invalid(
                "allowed_repo_ids entries must be 40-character Git object IDs".into(),
            ));
        }
        if self
            .hub_invite
            .as_ref()
            .is_some_and(|invite| invite.trim().is_empty())
        {
            return Err(ConfigError::Invalid("hub_invite must not be empty".into()));
        }
        if let Some(invite) = &self.hub_invite {
            tagteam_pki::Invite::decode(invite).map_err(|_| {
                ConfigError::Invalid("hub_invite is not a valid enrollment invite".into())
            })?;
        }
        Ok(())
    }
}

/// Return the conventional config file location when a home directory is available.
pub fn default_config_path() -> Result<Option<PathBuf>, ConfigError> {
    Ok(std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_CONFIG)))
}

fn default_name() -> String {
    "tagteam-node".into()
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("~/.local/share/tagteam-node")
}
fn default_idle_after() -> u64 {
    10 * 60
}
fn default_on_owner_return() -> OnReturn {
    OnReturn::Pause
}
fn default_max_subtasks() -> u32 {
    1
}
fn default_true() -> bool {
    true
}

fn deserialize_idle_after<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Seconds(u64),
        Human(String),
    }

    match Input::deserialize(deserializer)? {
        Input::Seconds(seconds) => Ok(seconds),
        Input::Human(value) => tagteam_core::human::parse_duration(&value)
            .map(|duration| duration.as_secs())
            .map_err(serde::de::Error::custom),
    }
}

fn deserialize_optional_bytes<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Bytes(u64),
        Human(String),
    }

    Option::<Input>::deserialize(deserializer)?
        .map(|input| match input {
            Input::Bytes(bytes) => Ok(bytes),
            Input::Human(value) => {
                tagteam_core::human::parse_bytes(&value).map_err(serde::de::Error::custom)
            }
        })
        .transpose()
}

fn expand_path(path: &Path) -> Result<PathBuf, ConfigError> {
    let value = path.to_string_lossy();
    if value == "~" || value.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            ConfigError::Invalid("HOME is unavailable for data_dir expansion".into())
        })?;
        let suffix = value.strip_prefix("~/").unwrap_or("");
        return Ok(PathBuf::from(home).join(suffix));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::NodeConfig;

    #[test]
    fn defaults_match_the_core_node_policy() {
        let config = NodeConfig::default();
        let policy = config.policy();
        assert!(!policy.only_when_idle);
        assert_eq!(policy.idle_after.as_secs(), 600);
        assert_eq!(policy.on_owner_return, tagteam_core::OnReturn::Pause);
        assert_eq!(policy.max_cores, None);
        assert_eq!(policy.max_subtasks, 1);
        assert!(policy.never_on_battery);
        assert_eq!(config.allowed_repo_ids, None);
    }

    #[test]
    fn config_parses_fields_and_rejects_invalid_limits() {
        let config: NodeConfig = toml::from_str("name='build-node'\nonly_when_idle=true\nidle_after=90\nmax_cores=4\nmax_memory=2048\nmax_subtasks=3\nnever_on_battery=false\nextra_hosts=['git.example']\n").expect("valid config parses");
        assert_eq!(config.name, "build-node");
        assert_eq!(config.idle_after, 90);
        assert_eq!(config.policy().max_cores, Some(4));
        assert_eq!(config.policy().max_memory, Some(tagteam_core::Bytes(2048)));

        let mut invalid = config;
        invalid.max_subtasks = 0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn config_accepts_friendly_durations_bytes_and_hour_ranges() {
        let config: NodeConfig =
            toml::from_str("idle_after='10m'\nmax_memory='12G'\nhours=['22:00-07:00']\n")
                .expect("friendly config parses");
        assert_eq!(config.idle_after, 600);
        assert_eq!(config.max_memory, Some(12 * 1024_u64.pow(3)));
        assert_eq!(
            config.hours,
            Some(vec![tagteam_core::HourRange {
                start_minutes: 1320,
                end_minutes: 420,
            }])
        );
        assert!(config.validate().is_ok());

        let struct_range: NodeConfig =
            toml::from_str("hours=[{start_minutes=1320,end_minutes=420}]\n")
                .expect("legacy hour range parses");
        assert_eq!(struct_range.hours, config.hours);

        let legacy: NodeConfig =
            toml::from_str("idle_after=90\nmax_memory=2048\n").expect("legacy values parse");
        assert_eq!(legacy.idle_after, 90);
        assert_eq!(legacy.max_memory, Some(2048));
    }

    #[test]
    fn config_rejects_ambiguous_or_overflowing_human_values() {
        assert!(toml::from_str::<NodeConfig>("idle_after='1.5h'\n").is_err());
        assert!(toml::from_str::<NodeConfig>("idle_after='18446744073709551615w'\n").is_err());
        assert!(toml::from_str::<NodeConfig>("max_memory='1.5G'\n").is_err());
        assert!(toml::from_str::<NodeConfig>("max_memory='18446744073709551615T'\n").is_err());
        assert!(toml::from_str::<NodeConfig>("hours=['24:00-07:00']\n").is_err());
    }

    #[test]
    fn repository_trust_is_explicit_for_borrowed_machines() {
        let config = NodeConfig {
            allowed_repo_ids: Some(Vec::new()),
            ..NodeConfig::default()
        };
        assert!(config.validate().is_ok());

        let config = NodeConfig {
            allowed_repo_ids: Some(vec!["not-an-oid".into()]),
            ..NodeConfig::default()
        };
        assert!(config.validate().is_err());

        let config = NodeConfig {
            allowed_repo_ids: Some(vec!["0123456789012345678901234567890123456789".into()]),
            ..NodeConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_hours_are_rejected() {
        let config: NodeConfig =
            toml::from_str("hours=[{start_minutes=1440,end_minutes=3}]\n").expect("shape parses");
        assert!(config.validate().is_err());
    }
}
