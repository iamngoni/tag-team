//! Tag Team hub daemon and local command-line client.
//!
//! The binary has two distinct trust surfaces: `hub` binds the mTLS node plane
//! and the private Unix socket, while every other command talks only to that
//! socket. No CLI command opens the SQLite journal or CA directly.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tagteam_core::{
    QuestionId, SubtaskId,
    local_api::{AnswerAuthor, LocalFrame, LocalRequest, LocalResponseBody, WatchScope},
};
use tagteam_hub::{
    config::HubConfig, local::client::LocalClient, mcp::McpServer, service::HubService,
};
use tagteam_pki::HubCa;
use tokio::sync::watch;

#[derive(Debug, Parser)]
#[command(name = "tagteam", version, about = "Tag Team hub and local client")]
struct Cli {
    /// Hub configuration file. Defaults to ~/.config/tagteam/hub.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the hub daemon.
    Hub,
    /// Run the stdio MCP shim for one Claude Code session.
    Mcp,
    /// Manage the per-user background service.
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Print the hub version.
    Version,
    /// Create a single-use node invite.
    Invite {
        name: String,
        #[arg(long, default_value = "10m")]
        ttl: String,
    },
    /// Approve an enrolled node.
    Approve { name: String },
    /// Pause an active node.
    Pause { name: String },
    /// Resume a paused node.
    Resume { name: String },
    /// Revoke a node certificate.
    Remove { name: String },
    /// List enrolled nodes.
    Nodes,
    /// Show subtask status.
    /// Print one stable state word, or a JSON object with `--json`.
    Status {
        #[arg(long)]
        id: Vec<SubtaskId>,
    },
    /// Read queued events.
    Inbox {
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long, default_value = "0s")]
        wait: String,
        #[arg(long)]
        kind: Vec<String>,
    },
    /// Follow committed events.
    Watch {
        #[arg(long)]
        dispatch: Option<tagteam_core::DispatchId>,
        #[arg(long)]
        subtask: Option<SubtaskId>,
        #[arg(long, default_value_t = 0)]
        since: u64,
    },
    /// Show rendered subtask events.
    Logs {
        subtask: SubtaskId,
        #[arg(long, default_value_t = 100)]
        tail: u32,
    },
    /// Cancel a non-terminal subtask.
    Cancel {
        subtask: SubtaskId,
        #[arg(long, default_value = "cancelled by user")]
        reason: String,
    },
    /// Answer the open question from a waiting subtask.
    Answer {
        subtask: SubtaskId,
        text: String,
        #[arg(long)]
        question: Option<QuestionId>,
        /// Answer author: `you` (default) or `main`.
        #[arg(long, default_value = "you")]
        by: String,
    },
    /// Steer a running subtask with a new instruction.
    Steer { subtask: SubtaskId, text: String },
    /// Show a completed result and optionally create a review worktree.
    Collect {
        subtask: SubtaskId,
        #[arg(long)]
        worktree: bool,
    },
    /// Print the one-line Claude session status hint.
    Brief { claude_session: String },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    Install,
    Start,
    Stop,
    Restart,
    Status {
        /// Print stable JSON for scripts.
        #[arg(long)]
        json: bool,
    },
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Version) {
        Command::Version => {
            println!("tagteam {}", tagteam_hub::HUB_VERSION);
            Ok(())
        }
        Command::Hub => run_hub(cli.config.as_deref()).await,
        Command::Mcp => run_mcp(cli.config.as_deref()).await,
        Command::Service { action } => run_service(action, cli.config.as_deref()),
        command => {
            let config = HubConfig::load(cli.config.as_deref())?;
            let paths = config.prepare_data_dir()?;
            let mut client = LocalClient::connect(&paths.socket)
                .await
                .context("is `tagteam hub` running?")?;
            run_client(command, &mut client).await
        }
    }
}

async fn run_mcp(config_path: Option<&std::path::Path>) -> Result<()> {
    let config = HubConfig::load(config_path)?;
    let paths = config.prepare_data_dir()?;
    let cwd = std::env::current_dir().context("resolve MCP session working directory")?;
    let server = McpServer::attach(&paths.socket, cwd)
        .await
        .context("attach MCP shim to `tagteam hub`")?;
    tagteam_hub::mcp::serve_stdio(server).await
}

async fn run_hub(config_path: Option<&std::path::Path>) -> Result<()> {
    let config = HubConfig::load(config_path)?;
    let paths = config.prepare_data_dir()?;
    let ca = Arc::new(
        HubCa::load_or_generate(&paths.ca_certificate, &paths.ca_private_key)
            .context("load persistent hub CA")?,
    );
    let service = HubService::open(&config, ca).await?;
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("bind node plane at {}", config.listen))?;
    let (shutdown, shutdown_rx) = watch::channel(false);

    let expiry_service = service.clone();
    let mut expiry_shutdown = shutdown_rx.clone();
    let expiry = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = expiry_shutdown.changed() => break,
                _ = ticker.tick() => {
                    if let Err(error) = expiry_service.expire_due_questions().await {
                        tracing::warn!(%error, "question expiry sweep failed");
                    }
                }
            }
        }
    });

    let reconciler_service = service.clone();
    let mut reconciler_shutdown = shutdown_rx.clone();
    let reconciler = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = reconciler_shutdown.changed() => break,
                _ = ticker.tick() => {
                    if let Err(error) = reconciler_service.reconcile_nodes().await {
                        tracing::warn!(%error, "node reconciliation sweep failed");
                    }
                }
            }
        }
    });

    let grpc_service = service.nodes().clone();
    let grpc_shutdown = shutdown_rx.clone();
    let mut grpc = tokio::spawn(async move {
        grpc_service
            .serve(listener, async move {
                let mut shutdown = grpc_shutdown;
                let _ = shutdown.changed().await;
            })
            .await
    });
    let local_service = service.clone();
    let local_shutdown = shutdown_rx;
    let socket_path = paths.socket.clone();
    let mut local = tokio::spawn(async move {
        tagteam_hub::local::server::serve(&socket_path, local_service, local_shutdown).await
    });

    tracing::info!(
        node_listen = %config.listen,
        socket = %paths.socket.display(),
        ca_fingerprint = %service.ca().fingerprint().to_hex(),
        "Tag Team hub started"
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("shutdown requested"),
        result = &mut grpc => {
            result.context("node plane task stopped")??;
            bail!("node plane stopped unexpectedly");
        }
        result = &mut local => {
            result.context("local API task stopped")??;
            bail!("local API stopped unexpectedly");
        }
    }
    shutdown.send_replace(true);
    let _ = grpc.await;
    let _ = local.await;
    let _ = expiry.await;
    let _ = reconciler.await;
    Ok(())
}

async fn run_client(command: Command, client: &mut LocalClient) -> Result<()> {
    match command {
        Command::Invite { name, ttl } => {
            let ttl_seconds = parse_ttl(&ttl)?;
            match request(client, LocalRequest::Invite { name, ttl_seconds }).await? {
                LocalResponseBody::Invited(reply) => {
                    println!("{}", reply.invite);
                    eprintln!(
                        "invite for {} expires in {} seconds",
                        reply.name, reply.expires_in_seconds
                    );
                }
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Approve { name } => node_change(client, LocalRequest::Approve { name }).await?,
        Command::Pause { name } => node_change(client, LocalRequest::Pause { name }).await?,
        Command::Resume { name } => node_change(client, LocalRequest::Resume { name }).await?,
        Command::Remove { name } => node_change(client, LocalRequest::Remove { name }).await?,
        Command::Nodes => match request(client, LocalRequest::Nodes).await? {
            LocalResponseBody::Nodes(nodes) => {
                if nodes.is_empty() {
                    println!("no nodes enrolled");
                }
                for view in nodes {
                    println!(
                        "{:<20} {:<10} {:>5.1}/{:<4} cores  {:>4} running  seen {}s ago  {}",
                        view.node.name,
                        node_state(&view.node.state),
                        view.node.capacity.cores_free,
                        view.node.capacity.cores_total,
                        view.running_subtasks.len(),
                        view.last_seen_age.as_secs(),
                        view.node.platform.os_version
                    );
                }
            }
            other => bail!("unexpected reply: {other:?}"),
        },
        Command::Status { id } => match request(
            client,
            LocalRequest::Status {
                ids: (!id.is_empty()).then_some(id),
                claude_session: None,
            },
        )
        .await?
        {
            LocalResponseBody::Status(reply) => {
                if reply.subtasks.is_empty() {
                    println!("no subtasks");
                }
                for subtask in reply.subtasks {
                    println!(
                        "{}  {:<10}  {:<30}  {}",
                        subtask.id,
                        subtask.state,
                        subtask.title,
                        subtask.branch.unwrap_or_default()
                    );
                    if let Some(progress) = subtask.last_progress {
                        println!("  {progress}");
                    }
                }
            }
            other => bail!("unexpected reply: {other:?}"),
        },
        Command::Inbox { since, wait, kind } => {
            let wait_ms = u64::try_from(parse_duration(&wait)?.as_millis()).unwrap_or(u64::MAX);
            let kinds = (!kind.is_empty()).then_some(kind);
            match request(
                client,
                LocalRequest::Inbox {
                    since_seq: since,
                    wait_ms,
                    kinds,
                    all: true,
                    claude_session: None,
                },
            )
            .await?
            {
                LocalResponseBody::Events(events) => print_events(&events)?,
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Watch {
            dispatch,
            subtask,
            since,
        } => {
            let scope = match (dispatch, subtask) {
                (Some(dispatch), None) => WatchScope::Dispatch(dispatch),
                (None, Some(subtask)) => WatchScope::Subtask(subtask),
                (None, None) => WatchScope::All,
                (Some(_), Some(_)) => bail!("choose --dispatch or --subtask, not both"),
            };
            client
                .send(
                    1,
                    LocalRequest::Watch {
                        scope,
                        since_seq: since,
                    },
                )
                .await?;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                    frame = client.next_frame() => match frame? {
                        Some(LocalFrame::Event { event, .. }) => print_events(&[event])?,
                        Some(LocalFrame::Response { .. }) => {}
                        Some(LocalFrame::Error { message, .. }) => bail!("{message}"),
                        Some(LocalFrame::Request { .. }) => bail!("daemon sent a request frame"),
                        None => return Ok(()),
                    }
                }
            }
        }
        Command::Logs { subtask, tail } => {
            match request(
                client,
                LocalRequest::Logs {
                    subtask,
                    tail,
                    claude_session: None,
                },
            )
            .await?
            {
                LocalResponseBody::Logs(reply) => {
                    for line in reply.lines {
                        println!("{line}");
                    }
                }
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Cancel { subtask, reason } => {
            match request(
                client,
                LocalRequest::Cancel {
                    subtask,
                    reason,
                    claude_session: None,
                },
            )
            .await?
            {
                LocalResponseBody::Cancelled { subtask } => println!("cancelled {subtask}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Answer {
            subtask,
            text,
            question,
            by,
        } => {
            let author = match by.trim().to_ascii_lowercase().as_str() {
                "you" | "user" => AnswerAuthor::You,
                "main" | "claude" => AnswerAuthor::MainClaude,
                _ => bail!("--by must be `you` or `main`"),
            };
            match request(
                client,
                LocalRequest::Answer {
                    subtask,
                    question,
                    text,
                    author,
                    claude_session: None,
                },
            )
            .await?
            {
                LocalResponseBody::Answered(reply) => {
                    println!("answered {}: {}", reply.question, reply.text);
                    if !reply.delivered {
                        eprintln!(
                            "delivery {} is queued and has not been written to the runner yet",
                            reply.delivery_id
                        );
                    } else if !reply.acknowledged {
                        eprintln!(
                            "delivery {} was sent and is awaiting runner acknowledgement",
                            reply.delivery_id
                        );
                    }
                }
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Steer { subtask, text } => {
            match request(
                client,
                LocalRequest::Steer {
                    subtask,
                    text,
                    claude_session: None,
                },
            )
            .await?
            {
                LocalResponseBody::Steered {
                    subtask,
                    delivered,
                    acknowledged,
                    ..
                } => {
                    println!(
                        "steered {subtask} (delivered: {delivered}, acknowledged: {acknowledged})"
                    )
                }
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Collect { subtask, worktree } => {
            match request(
                client,
                LocalRequest::Collect {
                    subtask,
                    worktree,
                    claude_session: None,
                },
            )
            .await?
            {
                LocalResponseBody::Collected(reply) => {
                    println!("{}", serde_json::to_string_pretty(&reply.result)?);
                    if let Some(path) = reply.worktree {
                        println!("worktree: {}", path.display());
                    }
                }
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Brief { claude_session } => {
            match request(client, LocalRequest::Brief { claude_session }).await? {
                LocalResponseBody::Brief(reply) => println!("{}", reply.display()),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Command::Hub | Command::Mcp | Command::Service { .. } | Command::Version => {
            bail!("command is handled before the local client")
        }
    }
    Ok(())
}

fn run_service(action: ServiceCommand, config_path: Option<&std::path::Path>) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot determine service paths"))?;
    let home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir()?.join(home)
    };
    let selected_config = config_path
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config/tagteam/hub.toml"));
    let selected_config = if selected_config.is_absolute() {
        selected_config
    } else {
        std::env::current_dir()?.join(selected_config)
    };
    let executable = std::env::current_exe()?.canonicalize()?;
    let log_dir = if cfg!(target_os = "macos") {
        home.join("Library/Logs/TagTeam")
    } else {
        home.join(".local/state/tagteam")
    };
    let spec = tagteam_service::ServiceSpec {
        label: "com.tagteam.hub".into(),
        executable,
        args: vec![
            "hub".into(),
            "--config".into(),
            selected_config.to_string_lossy().into_owned(),
        ],
        config_path: selected_config,
        stdout_path: log_dir.join("hub.out.log"),
        stderr_path: log_dir.join("hub.err.log"),
        working_directory: home,
    };
    let (action, json) = match action {
        ServiceCommand::Install => (tagteam_service::ServiceAction::Install, false),
        ServiceCommand::Start => (tagteam_service::ServiceAction::Start, false),
        ServiceCommand::Stop => (tagteam_service::ServiceAction::Stop, false),
        ServiceCommand::Restart => (tagteam_service::ServiceAction::Restart, false),
        ServiceCommand::Status { json } => (tagteam_service::ServiceAction::Status, json),
        ServiceCommand::Uninstall => (tagteam_service::ServiceAction::Uninstall, false),
    };
    let state = tagteam_service::manage(&spec, action)?;
    if json {
        println!(
            "{{\"label\":{},\"state\":{}}}",
            serde_json::to_string(&spec.label)?,
            serde_json::to_string(state.as_str())?
        );
    } else if matches!(action, tagteam_service::ServiceAction::Status) {
        println!("{}", state.as_str());
    } else {
        println!("service {}: {}", spec.label, state.as_str());
    }
    Ok(())
}

async fn request(client: &mut LocalClient, request: LocalRequest) -> Result<LocalResponseBody> {
    client.request(request).await.map_err(Into::into)
}

async fn node_change(client: &mut LocalClient, request: LocalRequest) -> Result<()> {
    match self::request(client, request).await? {
        LocalResponseBody::NodeChanged { name } => println!("{name} updated"),
        other => bail!("unexpected reply: {other:?}"),
    }
    Ok(())
}

fn print_events(events: &[tagteam_core::Event]) -> Result<()> {
    for event in events {
        println!("{}", serde_json::to_string(event)?);
    }
    Ok(())
}

fn node_state(state: &tagteam_core::NodeState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{state:?}"))
}

fn parse_ttl(value: &str) -> Result<u64> {
    let duration = parse_duration(value)?;
    let seconds = duration.as_secs();
    if seconds == 0 || seconds > 3_600 {
        bail!("TTL must be between 1 second and 1 hour");
    }
    Ok(seconds)
}

fn parse_duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(split);
    let number: u64 = number
        .parse()
        .context("duration must start with a number")?;
    let multiplier = match suffix {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => bail!("duration suffix must be s, m, or h"),
    };
    let seconds = number
        .checked_mul(multiplier)
        .context("duration is too large")?;
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod cli_tests {
    use super::{Cli, Command, ServiceCommand};
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn service_commands_accept_inherited_config_and_json_status() {
        let cli = Cli::try_parse_from([
            "tagteam",
            "service",
            "status",
            "--json",
            "--config",
            "/tmp/hub.toml",
        ])
        .expect("service status args parse");
        assert_eq!(cli.config.as_deref(), Some(Path::new("/tmp/hub.toml")));
        assert!(matches!(
            cli.command,
            Some(Command::Service {
                action: ServiceCommand::Status { json: true }
            })
        ));
        for verb in ["install", "start", "stop", "restart", "uninstall"] {
            assert!(Cli::try_parse_from(["tagteam", "service", verb]).is_ok());
        }
    }
}
