//! Node-owned enrollment and the outbound mTLS session client.
//!
//! This crate owns node-side transport identity and session lifecycle. Private key
//! persistence is delegated through [`IdentityStore`]; callers must never log identity
//! material, invites, or one-time enrollment codes.

mod enrollment;
pub mod fsop;
mod heartbeat;
mod identity_store;
mod journal;
mod runtime;
mod session;
mod transport;

pub use enrollment::{IdentityStore, IdentityStoreError, NodeEnrollment, NodeError, NodeIdentity};
pub use heartbeat::HostHeartbeat;
pub use identity_store::{FileIdentityError, FileIdentityStore};
pub use journal::{BeginCommand, CommandJournal, JournalError};
pub use runtime::{BundleSource, CommandRunner, NoBundleSource};
pub use session::{
    HeartbeatSource, NodeSession, PendingResultSource, ReconnectPolicy, SessionError, SessionEvent,
    SessionHandle, SessionSender,
};

pub const NODE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests;
