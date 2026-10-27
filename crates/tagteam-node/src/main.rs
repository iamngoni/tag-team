//! Node CLI and hidden confined helper entrypoints.
//!
//! Enrollment secrets stay in the protected identity store and never appear in CLI output;
//! pause/resume/leave are local connection controls because node lifecycle state is hub-owned.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Parser, Subcommand};
use tagteam_node::{
    CommandJournal, CommandRunner, FileIdentityStore, HeartbeatSource, HostHeartbeat,
    HubBundleSource, NodeEnrollment, NodeSession, SessionEvent,
};

#[derive(Debug, Parser)]
#[command(name = "tagteam-node", version, about = "Tag Team node")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enroll with an invite, persist config and identity, and optionally connect.
    Join {
        invite: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        run: bool,
    },
    /// Connect to the configured hub and serve commands.
    Run,
    /// Print local identity, effective config, and connection controls.
    Status,
    /// Stop a running local session and keep it stopped across restarts.
    Pause,
    /// Clear the local pause state; start it with `run` to reconnect.
    Resume,
    /// Permanently disable local reconnects while retaining identity for recovery.
    Leave,
    /// Print the exact heartbeat JSON without connecting.
    Probe,
    /// Print the node version.
    Version,
    #[command(name = "__fsop", hide = true)]
    Fsop { encoded: Option<String> },
    #[command(name = "__exec", hide = true)]
    Exec { encoded: Option<String> },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let config_path = cli.config.clone();
    let mut config = tagteam_node::config::NodeConfig::load(config_path.as_deref())?;
    if let Some(data_dir) = cli.data_dir {
        config.data_dir = data_dir;
    }
    match cli.command.unwrap_or(Command::Status) {
        Command::Version => println!("tagteam-node {}", tagteam_node::NODE_VERSION),
        Command::Join {
            invite,
            name,
            run: should_run,
        } => {
            if let Some(name) = name {
                config.name = name;
            }
            let invite = invite
                .or_else(|| config.hub_invite.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("join requires an invite argument or hub_invite in config")
                })?;
            join(&invite, &mut config, config_path.as_deref()).await?;
            if should_run {
                run(config).await?;
            }
        }
        Command::Run => run(config).await?,
        Command::Status => status(&config)?,
        Command::Pause => set_control(&config.data_dir, "paused", true)?,
        Command::Resume => {
            if is_left(&config.data_dir) {
                anyhow::bail!(
                    "node has left locally; remove the left control file to re-enable it"
                );
            }
            set_control(&config.data_dir, "paused", false)?;
        }
        Command::Leave => {
            set_control(&config.data_dir, "paused", false)?;
            set_control(&config.data_dir, "left", true)?;
            println!("local reconnect disabled; node identity retained");
        }
        Command::Probe => {
            let heartbeat = HostHeartbeat::with_policy(config.policy()).heartbeat();
            println!(
                "{}",
                serde_json::to_string_pretty(&heartbeat_json(heartbeat))?
            );
        }
        Command::Fsop { encoded } => {
            let response = match encoded {
                Some(encoded) => tagteam_node::fsop::run_encoded(&encoded),
                None => tagteam_node::fsop::run_encoded_from_reader(std::io::stdin().lock())
                    .map_err(anyhow::Error::msg)?,
            };
            println!("{response}");
        }
        Command::Exec { encoded } => {
            let code = match encoded {
                Some(encoded) => tagteam_node::fsop::run_exec_encoded(&encoded),
                None => tagteam_node::fsop::run_exec_encoded_from_reader(std::io::stdin().lock()),
            }
            .map_err(anyhow::Error::msg)?;
            std::process::exit(code);
        }
    }
    Ok(())
}

async fn join(
    invite: &str,
    config: &mut tagteam_node::config::NodeConfig,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let store = FileIdentityStore::new(&config.data_dir);
    if store.load()?.is_some() {
        anyhow::bail!("node identity already exists in the data directory");
    }
    let identity = NodeEnrollment::enroll(invite, config.name.clone(), current_platform()).await?;
    store.persist_with_invite(&identity, invite)?;
    config.hub_invite = None;
    let default_path = tagteam_node::config::default_config_path()?;
    if let Some(path) = config_path.or(default_path.as_deref()) {
        config.save(path)?;
    }
    tracing::info!(node_id = %identity.node_id, "node enrollment persisted");
    Ok(())
}

async fn run(config: tagteam_node::config::NodeConfig) -> anyhow::Result<()> {
    if is_left(&config.data_dir) {
        anyhow::bail!("node has left locally; remove the left control file to re-enable it");
    }
    if is_paused(&config.data_dir) {
        anyhow::bail!("node is locally paused; run `tagteam-node resume` first");
    }
    let store = FileIdentityStore::new(&config.data_dir);
    let (_, identity) = store.load()?.ok_or_else(|| {
        anyhow::anyhow!("node identity is missing; run `tagteam-node join` first")
    })?;
    let journal = CommandJournal::open(config.data_dir.join("commands.json"))?;
    let bundles = HubBundleSource::new(&identity)
        .await
        .map_err(anyhow::Error::msg)?;
    let runner = CommandRunner::new(&config.data_dir, journal.clone(), bundles);
    let policy = config.policy();
    let runner = runner.with_policy(policy.clone(), config.allowed_repo_ids.clone());
    let session = NodeSession::new(identity, HostHeartbeat::with_policy(policy.clone()))
        .pending_source(journal)
        .policy(policy)
        .start();
    let sender = session.sender();
    let mut session = session;
    let mut controls = tokio::time::interval(Duration::from_millis(500));
    loop {
        let event = tokio::select! {
            event = session.next_event() => event,
            _ = controls.tick() => {
                if is_paused(&config.data_dir) || is_left(&config.data_dir) {
                    session.shutdown();
                    break;
                }
                continue;
            }
        };
        let Some(event) = event else {
            break;
        };
        match event {
            SessionEvent::Welcome(welcome) => {
                tracing::info!(state = welcome.node_state, "connected to hub")
            }
            SessionEvent::Command(command) => runner.dispatch(*command, sender.clone()),
            SessionEvent::Cancel(cancel) => runner.cancel(&cancel.command_id),
            SessionEvent::Drain(_) => {
                session.shutdown();
                break;
            }
            SessionEvent::Reconnecting { attempt, delay } => {
                tracing::info!(attempt, ?delay, "reconnecting to hub")
            }
            SessionEvent::Terminated(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn heartbeat_json(heartbeat: tagteam_proto::v1::Heartbeat) -> serde_json::Value {
    let capacity = heartbeat.capacity.map(|value| {
        serde_json::json!({
            "cores_total": value.cores_total,
            "cores_free": value.cores_free,
            "memory_free": value.memory_free,
            "disk_free": value.disk_free,
            "running": value.running,
            "on_battery": value.on_battery,
            "thermal": value.thermal,
        })
    });
    serde_json::json!({
        "capacity": capacity,
        "presence": heartbeat.presence,
        "toolchains": heartbeat.toolchains.into_iter().map(|value| serde_json::json!({
            "name": value.name, "version": value.version, "path": value.path
        })).collect::<Vec<_>>(),
        "repos": heartbeat.repos.into_iter().map(|value| serde_json::json!({
            "repo_id": value.repo_id, "heads": value.heads
        })).collect::<Vec<_>>(),
        "running": heartbeat.running,
        "idle_for_ms": heartbeat.idle_for_ms,
    })
}

fn status(config: &tagteam_node::config::NodeConfig) -> anyhow::Result<()> {
    let store = FileIdentityStore::new(&config.data_dir);
    let identity = store.load()?.map(|(_, identity)| identity);
    let value = serde_json::json!({
        "name": config.name,
        "data_dir": config.data_dir,
        "node_id": identity.map(|identity| identity.node_id.to_string()),
        "enrolled": store.load()?.is_some(),
        "paused": is_paused(&config.data_dir),
        "left": is_left(&config.data_dir),
        "policy": config.policy(),
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn set_control(data_dir: &Path, name: &str, enabled: bool) -> anyhow::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = data_dir.join(format!("{name}.state"));
    if enabled {
        fs::write(path, b"enabled\n")?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    println!(
        "{}",
        if enabled {
            format!("local {name} enabled")
        } else {
            format!("local {name} cleared")
        }
    );
    Ok(())
}

fn is_paused(data_dir: &Path) -> bool {
    data_dir.join("paused.state").is_file()
}
fn is_left(data_dir: &Path) -> bool {
    data_dir.join("left.state").is_file()
}

fn current_platform() -> tagteam_proto::v1::Platform {
    let os = if cfg!(target_os = "macos") {
        tagteam_proto::v1::Os::Macos
    } else {
        tagteam_proto::v1::Os::Linux
    };
    let arch = if cfg!(target_arch = "aarch64") {
        tagteam_proto::v1::Arch::Arm64
    } else {
        tagteam_proto::v1::Arch::X64
    };
    tagteam_proto::v1::Platform {
        os: os as i32,
        arch: arch as i32,
        os_version: std::env::consts::OS.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn cli_accepts_operational_commands_and_join_run_option() {
        for command in ["run", "status", "pause", "resume", "leave", "probe"] {
            let cli = Cli::try_parse_from(["tagteam-node", command]).expect("valid command");
            assert!(cli.command.is_some());
        }
        let cli = Cli::try_parse_from([
            "tagteam-node",
            "join",
            "invite-code",
            "--name",
            "desk",
            "--run",
        ])
        .expect("join args parse");
        assert!(matches!(cli.command, Some(Command::Join { run: true, .. })));
    }
}
