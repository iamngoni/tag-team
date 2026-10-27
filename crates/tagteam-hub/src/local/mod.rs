//! Private Unix-socket transport shared by the Tag Team CLI and stdio MCP shim.
//!
//! Frames are length-delimited JSON and bounded to 1 MiB. The daemon rejects a
//! different peer UID, removes only stale socket files, and never exposes this
//! surface on the network.

pub mod client;
pub mod server;

#[cfg(test)]
mod tests;

use tokio_util::codec::LengthDelimitedCodec;

/// Maximum encoded local frame accepted in either direction.
pub const MAX_LOCAL_FRAME_BYTES: usize = 1024 * 1024;

pub(crate) fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_LOCAL_FRAME_BYTES)
        .length_field_length(4)
        .new_codec()
}
