//! Manages one per-user Tag Team service using native macOS or Linux service managers.
//!
//! This crate owns only the service definition and manager commands. It never edits application
//! data, identity, journals, or workspaces, and all subprocesses receive argument arrays directly.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// Complete, validated inputs used to describe one user-level service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSpec {
    /// Stable service label (macOS label or systemd unit stem).
    pub label: String,
    /// Absolute executable path.
    pub executable: PathBuf,
    /// Arguments passed to the executable without shell parsing.
    pub args: Vec<String>,
    /// Selected application configuration path.
    pub config_path: PathBuf,
    /// Standard output log path.
    pub stdout_path: PathBuf,
    /// Standard error log path.
    pub stderr_path: PathBuf,
    /// Process working directory.
    pub working_directory: PathBuf,
}

impl ServiceSpec {
    /// Validate an explicit service definition before any filesystem or process side effect.
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.label.is_empty()
            || !self
                .label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(ServiceError::InvalidSpec(
                "label must contain only ASCII letters, digits, '.', '-' or '_'".into(),
            ));
        }
        for (name, path) in [
            ("executable", &self.executable),
            ("config path", &self.config_path),
            ("stdout log path", &self.stdout_path),
            ("stderr log path", &self.stderr_path),
            ("working directory", &self.working_directory),
        ] {
            if !path.is_absolute() {
                return Err(ServiceError::InvalidSpec(format!(
                    "{name} must be an absolute path"
                )));
            }
            path_as_str(path, name)?;
        }
        for arg in &self.args {
            if arg.contains('\0') {
                return Err(ServiceError::InvalidSpec(
                    "arguments must not contain NUL bytes".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Stable lifecycle state suitable for human or JSON output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceState {
    /// No service definition is installed for the spec.
    NotInstalled,
    /// Installed and currently running.
    Running,
    /// Installed but not running.
    Stopped,
    /// The manager reports a failed service.
    Failed,
}

impl ServiceState {
    /// Stable lowercase identifier for scripts and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// Errors returned while validating or managing a service.
#[derive(Debug)]
pub enum ServiceError {
    /// The spec contains an invalid label or path.
    InvalidSpec(String),
    /// Filesystem access to a service definition failed.
    Io(io::Error),
    /// A native service manager command failed.
    Command {
        program: &'static str,
        detail: String,
    },
    /// The current target operating system has no supported user service manager.
    Unsupported(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSpec(message) => write!(f, "invalid service spec: {message}"),
            Self::Io(error) => write!(f, "service filesystem operation failed: {error}"),
            Self::Command { program, detail } => {
                write!(f, "service manager command `{program}` failed: {detail}")
            }
            Self::Unsupported(platform) => {
                write!(f, "user services are unsupported on platform `{platform}`")
            }
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidSpec(_) | Self::Command { .. } | Self::Unsupported(_) => None,
        }
    }
}

impl From<io::Error> for ServiceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Operations supported for a single service definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceAction {
    /// Write and register the per-user service definition.
    Install,
    /// Start the installed service.
    Start,
    /// Stop the service if running.
    Stop,
    /// Restart the service.
    Restart,
    /// Read the current service state.
    Status,
    /// Stop and remove the service definition.
    Uninstall,
}

/// Return the native definition path for this spec on supported platforms.
pub fn definition_path(spec: &ServiceSpec) -> Result<PathBuf, ServiceError> {
    spec.validate()?;
    platform::definition_path(&spec.label)
}

/// Render a launchd property list without touching the filesystem.
pub fn render_launch_agent(spec: &ServiceSpec) -> Result<String, ServiceError> {
    spec.validate()?;
    let mut arguments = Vec::with_capacity(spec.args.len() + 1);
    arguments.push(path_as_str(&spec.executable, "executable")?);
    arguments.extend(spec.args.iter().map(String::as_str));
    let mut output = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    xml_entry(&mut output, "Label", &spec.label);
    output.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for argument in arguments {
        output.push_str("    <string>");
        output.push_str(&xml_escape(argument));
        output.push_str("</string>\n");
    }
    output.push_str("  </array>\n");
    xml_entry(
        &mut output,
        "WorkingDirectory",
        path_as_str(&spec.working_directory, "working directory")?,
    );
    xml_entry(
        &mut output,
        "StandardOutPath",
        path_as_str(&spec.stdout_path, "stdout log path")?,
    );
    xml_entry(
        &mut output,
        "StandardErrorPath",
        path_as_str(&spec.stderr_path, "stderr log path")?,
    );
    output.push_str(
        "  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n</dict>\n</plist>\n",
    );
    Ok(output)
}

/// Render a systemd user unit without touching the filesystem.
pub fn render_systemd_unit(spec: &ServiceSpec) -> Result<String, ServiceError> {
    spec.validate()?;
    let mut output = String::from(
        "[Unit]\nDescription=Tag Team user service\n\n[Service]\nType=simple\nRestart=on-failure\n",
    );
    output.push_str("WorkingDirectory=");
    output.push_str(&systemd_escape(path_as_str(
        &spec.working_directory,
        "working directory",
    )?)?);
    output.push_str("\nExecStart=");
    output.push_str(&systemd_escape(path_as_str(
        &spec.executable,
        "executable",
    )?)?);
    for arg in &spec.args {
        output.push(' ');
        output.push_str(&systemd_escape(arg)?);
    }
    output.push_str("\nStandardOutput=append:");
    output.push_str(&systemd_escape(path_as_str(
        &spec.stdout_path,
        "stdout log path",
    )?)?);
    output.push_str("\nStandardError=append:");
    output.push_str(&systemd_escape(path_as_str(
        &spec.stderr_path,
        "stderr log path",
    )?)?);
    output.push_str("\n\n[Install]\nWantedBy=default.target\n");
    Ok(output)
}

/// Run one lifecycle operation through the native per-user service manager.
pub fn manage(spec: &ServiceSpec, action: ServiceAction) -> Result<ServiceState, ServiceError> {
    spec.validate()?;
    let state = platform::manage(spec, action)?;
    if matches!(action, ServiceAction::Status | ServiceAction::Uninstall) {
        Ok(state)
    } else {
        platform::manage(spec, ServiceAction::Status)
    }
}

fn path_as_str<'a>(path: &'a Path, name: &str) -> Result<&'a str, ServiceError> {
    path.to_str().ok_or_else(|| {
        ServiceError::InvalidSpec(format!(
            "{name} must be valid UTF-8 for the service manager"
        ))
    })
}

fn xml_entry(output: &mut String, key: &str, value: &str) {
    output.push_str("  <key>");
    output.push_str(key);
    output.push_str("</key>\n  <string>");
    output.push_str(&xml_escape(value));
    output.push_str("</string>\n");
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_escape(value: &str) -> Result<String, ServiceError> {
    if value.chars().any(|character| character.is_control()) {
        return Err(ServiceError::InvalidSpec(
            "systemd values must not contain control characters".into(),
        ));
    }
    let value = value.replace('%', "%%");
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('"');
    Ok(escaped)
}

fn private_dir(path: &Path) -> Result<(), ServiceError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), ServiceError> {
    let parent = path.parent().ok_or_else(|| {
        ServiceError::InvalidSpec("service definition path has no parent directory".into())
    })?;
    private_dir(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn prepare_log_directories(spec: &ServiceSpec) -> Result<(), ServiceError> {
    for log_path in [&spec.stdout_path, &spec.stderr_path] {
        let parent = log_path
            .parent()
            .ok_or_else(|| ServiceError::InvalidSpec("log path has no parent directory".into()))?;
        private_dir(parent)?;
    }
    Ok(())
}

fn output_text(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if text.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        text
    }
}

fn command(program: &'static str, args: &[&OsStr]) -> Result<Output, ServiceError> {
    Command::new(program)
        .args(args)
        .output()
        .map_err(|error| ServiceError::Command {
            program,
            detail: error.to_string(),
        })
}

fn check_command(program: &'static str, args: &[&OsStr]) -> Result<Output, ServiceError> {
    let output = command(program, args)?;
    if !output.status.success() {
        return Err(ServiceError::Command {
            program,
            detail: output_text(&output),
        });
    }
    Ok(output)
}

#[cfg(any(target_os = "linux", test))]
fn parse_systemd_active_state(value: &str) -> ServiceState {
    match value.trim() {
        "active" | "activating" | "reloading" => ServiceState::Running,
        "failed" => ServiceState::Failed,
        _ => ServiceState::Stopped,
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_launchctl_state(output: &str) -> ServiceState {
    if output.lines().any(|line| line.trim() == "state = running") {
        ServiceState::Running
    } else {
        ServiceState::Stopped
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub(super) fn definition_path(label: &str) -> Result<PathBuf, ServiceError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            ServiceError::InvalidSpec("HOME is not set for LaunchAgent installation".into())
        })?;
        Ok(PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist")))
    }

    fn domain() -> Result<String, ServiceError> {
        let output = command("id", &[OsStr::new("-u")])?;
        if !output.status.success() {
            return Err(ServiceError::Command {
                program: "id",
                detail: output_text(&output),
            });
        }
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ServiceError::InvalidSpec(
                "could not determine the current user id for LaunchAgent management".into(),
            ));
        }
        Ok(format!("gui/{uid}"))
    }

    pub(super) fn manage(
        spec: &ServiceSpec,
        action: ServiceAction,
    ) -> Result<ServiceState, ServiceError> {
        let path = definition_path(&spec.label)?;
        let target = format!("{}/{}", domain()?, spec.label);
        match action {
            ServiceAction::Install => {
                prepare_log_directories(spec)?;
                atomic_write(&path, &render_launch_agent(spec)?)?;
                let domain = domain()?;
                if command("launchctl", &[OsStr::new("print"), OsStr::new(&target)])?
                    .status
                    .success()
                {
                    check_command("launchctl", &[OsStr::new("bootout"), OsStr::new(&target)])?;
                }
                check_command(
                    "launchctl",
                    &[
                        OsStr::new("bootstrap"),
                        OsStr::new(&domain),
                        path.as_os_str(),
                    ],
                )?;
            }
            ServiceAction::Start => {
                if !path.exists() {
                    return Err(ServiceError::InvalidSpec(
                        "service is not installed; run install first".into(),
                    ));
                }
                if command("launchctl", &[OsStr::new("print"), OsStr::new(&target)])?
                    .status
                    .success()
                {
                    check_command(
                        "launchctl",
                        &[
                            OsStr::new("kickstart"),
                            OsStr::new("-k"),
                            OsStr::new(&target),
                        ],
                    )?;
                } else {
                    check_command(
                        "launchctl",
                        &[
                            OsStr::new("bootstrap"),
                            OsStr::new(&domain()?),
                            path.as_os_str(),
                        ],
                    )?;
                }
            }
            ServiceAction::Stop => {
                // `bootout` fails when the job is installed but not loaded. Stop
                // is idempotent, so an absent loaded job already satisfies it.
                if command("launchctl", &[OsStr::new("print"), OsStr::new(&target)])?
                    .status
                    .success()
                {
                    check_command("launchctl", &[OsStr::new("bootout"), OsStr::new(&target)])?;
                }
            }
            ServiceAction::Restart => {
                if command("launchctl", &[OsStr::new("print"), OsStr::new(&target)])?
                    .status
                    .success()
                {
                    check_command("launchctl", &[OsStr::new("bootout"), OsStr::new(&target)])?;
                }
                check_command(
                    "launchctl",
                    &[
                        OsStr::new("bootstrap"),
                        OsStr::new(&domain()?),
                        path.as_os_str(),
                    ],
                )?;
            }
            ServiceAction::Status => {
                if !path.exists() {
                    return Ok(ServiceState::NotInstalled);
                }
                let output = command("launchctl", &[OsStr::new("print"), OsStr::new(&target)])?;
                if !output.status.success() {
                    return Ok(ServiceState::Stopped);
                }
                return Ok(parse_launchctl_state(&String::from_utf8_lossy(
                    &output.stdout,
                )));
            }
            ServiceAction::Uninstall => {
                if path.exists() {
                    if command("launchctl", &[OsStr::new("print"), OsStr::new(&target)])?
                        .status
                        .success()
                    {
                        check_command("launchctl", &[OsStr::new("bootout"), OsStr::new(&target)])?;
                    }
                    fs::remove_file(&path)?;
                }
            }
        }
        if action == ServiceAction::Uninstall {
            Ok(ServiceState::NotInstalled)
        } else {
            Ok(ServiceState::Stopped)
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub(super) fn definition_path(label: &str) -> Result<PathBuf, ServiceError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            ServiceError::InvalidSpec("HOME is not set for systemd user installation".into())
        })?;
        Ok(PathBuf::from(home)
            .join(".config/systemd/user")
            .join(format!("{label}.service")))
    }

    pub(super) fn manage(
        spec: &ServiceSpec,
        action: ServiceAction,
    ) -> Result<ServiceState, ServiceError> {
        let path = definition_path(&spec.label)?;
        let unit = format!("{}.service", spec.label);
        match action {
            ServiceAction::Install => {
                prepare_log_directories(spec)?;
                atomic_write(&path, &render_systemd_unit(spec)?)?;
                check_command(
                    "systemctl",
                    &[OsStr::new("--user"), OsStr::new("daemon-reload")],
                )?;
                check_command(
                    "systemctl",
                    &[
                        OsStr::new("--user"),
                        OsStr::new("enable"),
                        OsStr::new(&unit),
                    ],
                )?;
            }
            ServiceAction::Start => {
                check_command(
                    "systemctl",
                    &[OsStr::new("--user"), OsStr::new("start"), OsStr::new(&unit)],
                )?;
            }
            ServiceAction::Stop => {
                check_command(
                    "systemctl",
                    &[OsStr::new("--user"), OsStr::new("stop"), OsStr::new(&unit)],
                )?;
            }
            ServiceAction::Restart => {
                check_command(
                    "systemctl",
                    &[
                        OsStr::new("--user"),
                        OsStr::new("restart"),
                        OsStr::new(&unit),
                    ],
                )?;
            }
            ServiceAction::Status => {
                if !path.exists() {
                    return Ok(ServiceState::NotInstalled);
                }
                let output = command(
                    "systemctl",
                    &[
                        OsStr::new("--user"),
                        OsStr::new("show"),
                        OsStr::new("--property=ActiveState"),
                        OsStr::new("--value"),
                        OsStr::new(&unit),
                    ],
                )?;
                if !output.status.success() {
                    return Err(ServiceError::Command {
                        program: "systemctl",
                        detail: output_text(&output),
                    });
                }
                return Ok(parse_systemd_active_state(&String::from_utf8_lossy(
                    &output.stdout,
                )));
            }
            ServiceAction::Uninstall => {
                if path.exists() {
                    check_command(
                        "systemctl",
                        &[
                            OsStr::new("--user"),
                            OsStr::new("disable"),
                            OsStr::new("--now"),
                            OsStr::new(&unit),
                        ],
                    )?;
                    fs::remove_file(&path)?;
                    check_command(
                        "systemctl",
                        &[OsStr::new("--user"), OsStr::new("daemon-reload")],
                    )?;
                }
            }
        }
        if action == ServiceAction::Uninstall {
            Ok(ServiceState::NotInstalled)
        } else {
            Ok(ServiceState::Stopped)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;

    pub(super) fn definition_path(_label: &str) -> Result<PathBuf, ServiceError> {
        Err(ServiceError::Unsupported(std::env::consts::OS.into()))
    }

    pub(super) fn manage(
        _spec: &ServiceSpec,
        _action: ServiceAction,
    ) -> Result<ServiceState, ServiceError> {
        Err(ServiceError::Unsupported(std::env::consts::OS.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            label: "com.example.tagteam".into(),
            executable: "/opt/Tag Team/tagteam".into(),
            args: vec![
                "hub".into(),
                "--config".into(),
                "/home/user/a&b.toml".into(),
            ],
            config_path: "/home/user/a&b.toml".into(),
            stdout_path: "/home/user/.local/state/tagteam/hub.out".into(),
            stderr_path: "/home/user/.local/state/tagteam/hub.err".into(),
            working_directory: "/home/user".into(),
        }
    }

    #[test]
    fn launch_agent_has_program_arguments_logs_and_xml_escaping() {
        let rendered = render_launch_agent(&spec()).expect("valid plist");
        assert!(rendered.contains("<string>/opt/Tag Team/tagteam</string>"));
        assert!(rendered.contains("<string>/home/user/a&amp;b.toml</string>"));
        assert!(rendered.contains("<key>StandardOutPath</key>"));
        assert!(rendered.contains("<key>StandardErrorPath</key>"));
        assert!(rendered.contains("<array>"));
    }

    #[test]
    fn systemd_unit_quotes_arguments_and_paths() {
        let rendered = render_systemd_unit(&spec()).expect("valid unit");
        assert!(rendered.contains(
            "ExecStart=\"/opt/Tag Team/tagteam\" \"hub\" \"--config\" \"/home/user/a&b.toml\""
        ));
        assert!(rendered.contains("WorkingDirectory=\"/home/user\""));
        assert!(
            rendered.contains("StandardOutput=append:\"/home/user/.local/state/tagteam/hub.out\"")
        );
        assert!(rendered.contains("WantedBy=default.target"));
    }

    #[test]
    fn validation_rejects_bad_labels_and_relative_paths() {
        let mut invalid = spec();
        invalid.label = "../tagteam".into();
        assert!(matches!(
            invalid.validate(),
            Err(ServiceError::InvalidSpec(_))
        ));
        let mut invalid = spec();
        invalid.executable = "tagteam".into();
        assert!(matches!(
            invalid.validate(),
            Err(ServiceError::InvalidSpec(_))
        ));
    }

    #[test]
    fn systemd_escaping_blocks_specifiers_and_quotes_syntax() {
        assert_eq!(
            systemd_escape("a% b\\c\"d").expect("valid"),
            "\"a%% b\\\\c\\\"d\""
        );
        assert!(systemd_escape("bad\nvalue").is_err());
    }

    #[test]
    fn parses_manager_state_values() {
        assert_eq!(
            parse_systemd_active_state("active\n"),
            ServiceState::Running
        );
        assert_eq!(parse_systemd_active_state("failed\n"), ServiceState::Failed);
        assert_eq!(
            parse_systemd_active_state("inactive\n"),
            ServiceState::Stopped
        );
        assert_eq!(
            parse_launchctl_state("state = running\n"),
            ServiceState::Running
        );
        assert_eq!(
            parse_launchctl_state("state = waiting\n"),
            ServiceState::Stopped
        );
    }
}
