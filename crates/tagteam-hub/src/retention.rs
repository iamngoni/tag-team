//! Applies the hub's event and blob age policies.
//!
//! SQLite event deletion is delegated to one journal transaction; blob deletion
//! is limited to old regular content-addressed files absent from journal refs.

use std::time::{Duration, UNIX_EPOCH};

use thiserror::Error;

use crate::{
    blobs::{BlobPurgeCounts, BlobStore},
    config::RetentionConfig,
    journal::{Journal, JournalError},
};
use tagteam_core::Timestamp;

/// Retention sweep failures are returned to startup and the periodic caller.
#[derive(Debug, Error)]
pub enum RetentionError {
    #[error("journal retention failed: {0}")]
    Journal(#[from] JournalError),
    #[error("blob retention failed: {0}")]
    Blob(#[from] crate::blobs::BlobError),
    #[error("retention age exceeds the supported timestamp range")]
    CutoffRange,
}

/// Counts removed during one retention sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionCounts {
    /// Events removed from terminal subtasks.
    pub events: u64,
    /// Unreferenced blobs removed from disk.
    pub blobs: u64,
    /// Empty blob shard directories removed from disk.
    pub shards: u64,
}

/// Runs both configured retention policies using a single supplied clock sample.
pub(crate) fn sweep(
    journal: &mut Journal,
    blobs: &BlobStore,
    policy: RetentionConfig,
    now: Timestamp,
) -> Result<RetentionCounts, RetentionError> {
    let references = journal.referenced_blob_digests()?;
    let events = journal.prune_events_before(timestamp_cutoff(now, policy.events)?)?;
    let now_system = UNIX_EPOCH
        .checked_add(Duration::from_millis(
            u64::try_from(now.0).map_err(|_| RetentionError::CutoffRange)?,
        ))
        .ok_or(RetentionError::CutoffRange)?;
    let blob_cutoff = now_system
        .checked_sub(policy.blobs)
        .ok_or(RetentionError::CutoffRange)?;
    let BlobPurgeCounts { blobs, shards } = blobs.purge_before(blob_cutoff, &references)?;
    Ok(RetentionCounts {
        events,
        blobs,
        shards,
    })
}

fn timestamp_cutoff(now: Timestamp, age: Duration) -> Result<Timestamp, RetentionError> {
    let age_millis = i64::try_from(age.as_millis()).map_err(|_| RetentionError::CutoffRange)?;
    now.0
        .checked_sub(age_millis)
        .map(Timestamp)
        .ok_or(RetentionError::CutoffRange)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_cutoff_uses_milliseconds_and_checks_range() {
        assert_eq!(
            timestamp_cutoff(Timestamp(10_000), Duration::from_secs(3)).expect("cutoff"),
            Timestamp(7_000)
        );
        assert_eq!(
            timestamp_cutoff(Timestamp(0), Duration::from_secs(1)).expect("negative Unix time"),
            Timestamp(-1_000)
        );
        assert!(timestamp_cutoff(Timestamp(i64::MIN), Duration::from_secs(1)).is_err());
        assert!(timestamp_cutoff(Timestamp(i64::MAX), Duration::MAX).is_err());
    }
}
