//! Node CLI and hidden confined helper entrypoints.
//!
//! Enrollment secrets are never emitted to logs; the visible join command stores the verified
//! identity before starting the reconnecting session and command dispatcher.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tagteam_node::{
    CommandJournal, CommandRunner, FileIdentityStore, HostHeartbeat, IdentityStore, NoBundleSource,
    NodeEnrollment, NodeSession, SessionEvent,
};

#[derive(Debug, Parser)]
#[command(name = "tagteam-node", version, about = "Tag Team node")]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enroll with an invite and keep the node connected to its hub.
    Join {
        invite: String,
        #[arg(long)]
        name: String,
    },
    /// Print the node version.
    Version,
    #[command(name = "__fsop", hide = true)]
    Fsop { encoded: String },
    #[command(name = "__exec", hide = true)]
    Exec { encoded: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let data_dir = match cli.data_dir {
        Some(path) => path,
        None => FileIdentityStore::default_data_dir()?,
    };
    match cli.command.unwrap_or(Command::Version) {
        Command::Version => println!("tagteam-node {}", tagteam_node::NODE_VERSION),
        Command::Join { invite, name } => join(&invite, name, data_dir).await?,
        Command::Fsop { encoded } => println!("{}", tagteam_node::fsop::run_encoded(&encoded)),
        Command::Exec { encoded } => {
            let code =
                tagteam_node::fsop::run_exec_encoded(&encoded).map_err(anyhow::Error::msg)?;
            std::process::exit(code);
        }
    }
    Ok(())
}

async fn join(invite: &str, name: String, data_dir: PathBuf) -> anyhow::Result<()> {
    let store = FileIdentityStore::new(&data_dir);
    if store.load()?.is_some() {
        anyhow::bail!("node identity already exists in the data directory");
    }
    let platform = current_platform();
    let identity = NodeEnrollment::enroll(invite, name, platform).await?;
    store.persist(&identity)?;
    let journal = CommandJournal::open(data_dir.join("commands.json"))?;
    let runner = CommandRunner::new(&data_dir, journal.clone(), NoBundleSource);
    let session = NodeSession::new(identity, HostHeartbeat)
        .pending_source(journal)
        .start();
    let sender = session.sender();
    let mut session = session;
    while let Some(event) = session.next_event().await {
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
