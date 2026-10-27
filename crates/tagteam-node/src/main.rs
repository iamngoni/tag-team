use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "tagteam-node", version, about = "Tag Team node")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the node version.
    Version,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Version) {
        Command::Version => println!("tagteam-node {}", tagteam_node::NODE_VERSION),
    }
    Ok(())
}
