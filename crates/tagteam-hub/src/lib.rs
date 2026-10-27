//! Tag Team hub: local API, scheduler, journal, MCP shim and node control plane.

pub const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod blobs;
pub mod config;
pub mod journal;
pub mod nodes;
pub mod runner;
