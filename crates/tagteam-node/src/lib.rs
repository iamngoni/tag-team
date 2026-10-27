//! Node-owned enrollment and the outbound mTLS session client.
//!
//! This crate owns node-side transport identity and session lifecycle. Private key
//! persistence is delegated through [`IdentityStore`]; callers must never log identity
//! material, invites, or one-time enrollment codes.

mod enrollment;
mod session;
mod transport;

pub use enrollment::{IdentityStore, IdentityStoreError, NodeEnrollment, NodeError, NodeIdentity};
pub use session::{
    HeartbeatSource, NodeSession, ReconnectPolicy, SessionError, SessionEvent, SessionHandle,
};

pub const NODE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests;
