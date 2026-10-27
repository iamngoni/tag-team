//! Git CLI wrappers for snapshotting, bundles, fetching and worktrees.

pub const SECRET_DENYLIST: &[&str] = &[".env", ".env.*", "*.pem", "*.key", "id_*"];
