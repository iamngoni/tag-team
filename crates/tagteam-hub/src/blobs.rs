//! Atomic content-addressed blob storage with verification on every read.

use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use sha2::{Digest, Sha256};
use tagteam_core::BlobRef;
use tempfile::NamedTempFile;
use thiserror::Error;

#[derive(Debug, Error)]
/// Blob storage failures, including content integrity mismatches.
pub enum BlobError {
    #[error("blob I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("blob digest does not match its content-addressed path")]
    DigestMismatch,
    #[error("blob size does not match its stored reference")]
    SizeMismatch,
    #[error("blob store root is not a regular directory")]
    InvalidRoot,
}

/// Counts returned by a content-addressed blob retention sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlobPurgeCounts {
    /// Number of old unreferenced regular blob files removed.
    pub blobs: u64,
    /// Number of empty two-character shard directories removed.
    pub shards: u64,
}

/// Files are named by SHA-256 and are made visible only after a synced write.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Opens a content store, creating its root directory when needed.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, BlobError> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
        })
    }

    /// Writes bytes atomically under their SHA-256 address and returns its ref.
    pub fn put(&self, bytes: &[u8]) -> Result<BlobRef, BlobError> {
        let sha256: [u8; 32] = Sha256::digest(bytes).into();
        let reference = BlobRef {
            sha256,
            size: bytes.len() as u64,
        };
        let destination = self.path_for(&reference);
        let parent = destination
            .parent()
            .ok_or_else(|| std::io::Error::other("blob path has no parent"))?;
        fs::create_dir_all(parent)?;
        if destination.exists() {
            self.read(&reference)?;
            return Ok(reference);
        }
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        let mut verify = Vec::new();
        temporary.reopen()?.read_to_end(&mut verify)?;
        verify_ref(&reference, &verify)?;
        let file = temporary
            .persist(&destination)
            .map_err(|error| error.error)?;
        file.sync_all()?;
        fs::File::open(&self.root)?.sync_all()?;
        Ok(reference)
    }

    /// Reads a blob only after verifying its byte count and SHA-256 digest.
    pub fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        let bytes = fs::read(self.path_for(reference))?;
        verify_ref(reference, &bytes)?;
        Ok(bytes)
    }

    /// Removes old, unreferenced content-addressed files without following
    /// symlinks, then removes empty shard directories.
    pub fn purge_before(
        &self,
        cutoff: SystemTime,
        referenced: &HashSet<[u8; 32]>,
    ) -> Result<BlobPurgeCounts, BlobError> {
        let root_type = fs::symlink_metadata(&self.root)?.file_type();
        if !root_type.is_dir() {
            return Err(BlobError::InvalidRoot);
        }

        let mut counts = BlobPurgeCounts::default();
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            let shard_name = shard.file_name();
            let Some(shard_name) = shard_name.to_str() else {
                continue;
            };
            if !is_shard_name(shard_name) || !shard.file_type()?.is_dir() {
                continue;
            }

            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some(digest) = parse_content_digest(&name, shard_name) else {
                    continue;
                };
                if referenced.contains(&digest) {
                    continue;
                }
                let metadata = fs::symlink_metadata(entry.path())?;
                if !metadata.file_type().is_file() || metadata.modified()? >= cutoff {
                    continue;
                }
                match fs::remove_file(entry.path()) {
                    Ok(()) => counts.blobs += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                }
            }

            let shard_path = shard.path();
            if fs::read_dir(&shard_path)?.next().is_none()
                && fs::symlink_metadata(&shard_path)?.file_type().is_dir()
            {
                match fs::remove_dir(&shard_path) {
                    Ok(()) => counts.shards += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(counts)
    }

    fn path_for(&self, reference: &BlobRef) -> PathBuf {
        let digest = reference.digest_hex();
        self.root.join(&digest[..2]).join(digest)
    }
}

fn is_shard_name(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn parse_content_digest(name: &str, shard: &str) -> Option<[u8; 32]> {
    if name.len() != 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || name.get(..2)? != shard
    {
        return None;
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(name.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(digest)
}

fn verify_ref(reference: &BlobRef, bytes: &[u8]) -> Result<(), BlobError> {
    if bytes.len() as u64 != reference.size {
        return Err(BlobError::SizeMismatch);
    }
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    if digest != reference.sha256 {
        return Err(BlobError::DigestMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_addressed_write_and_verified_read() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(directory.path().join("blobs")).expect("store");
        let reference = store.put(b"durable bytes").expect("write");
        assert_eq!(reference.size, 13);
        assert_eq!(store.read(&reference).expect("read"), b"durable bytes");
        assert_eq!(
            store.put(b"durable bytes").expect("idempotent write"),
            reference
        );
    }

    #[test]
    fn read_rejects_content_that_does_not_match_digest() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(directory.path()).expect("store");
        let reference = store.put(b"original").expect("write");
        fs::write(store.path_for(&reference), b"tampered").expect("tamper");
        assert!(matches!(
            store.read(&reference),
            Err(BlobError::DigestMismatch)
        ));
    }

    #[test]
    fn purge_removes_only_old_unreferenced_blobs_and_empty_shards() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(directory.path().join("blobs")).expect("store");
        let referenced = store.put(b"referenced").expect("referenced blob");
        let old_orphan = store.put(b"old orphan").expect("old orphan");
        let recent = store.put(b"recent").expect("recent blob");
        set_modified(store.path_for(&old_orphan), SystemTime::UNIX_EPOCH);

        let mut references = HashSet::new();
        references.insert(referenced.sha256);
        let cutoff = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(3_600))
            .expect("cutoff");
        let counts = store.purge_before(cutoff, &references).expect("purge");

        assert_eq!(counts.blobs, 1);
        assert!(!store.path_for(&old_orphan).exists());
        assert!(store.path_for(&referenced).exists());
        assert!(store.path_for(&recent).exists());
    }

    #[test]
    fn purge_removes_empty_digest_shards() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(directory.path().join("blobs")).expect("store");
        let orphan = store.put(b"only blob in this shard").expect("orphan");
        let shard = store
            .path_for(&orphan)
            .parent()
            .expect("shard")
            .to_path_buf();
        set_modified(store.path_for(&orphan), SystemTime::UNIX_EPOCH);

        let counts = store
            .purge_before(SystemTime::now(), &HashSet::new())
            .expect("purge");

        assert_eq!(counts.blobs, 1);
        assert_eq!(counts.shards, 1);
        assert!(!shard.exists());
    }

    #[cfg(unix)]
    #[test]
    fn purge_skips_symlinks_without_touching_their_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(directory.path().join("blobs")).expect("store");
        let reference = store.put(b"link target").expect("blob");
        let blob_path = store.path_for(&reference);
        fs::remove_file(&blob_path).expect("remove regular blob");
        let target = directory.path().join("outside");
        fs::write(&target, b"keep me").expect("target");
        symlink(&target, &blob_path).expect("symlink");

        let counts = store
            .purge_before(SystemTime::now(), &HashSet::new())
            .expect("purge");

        assert_eq!(counts.blobs, 0);
        assert!(
            fs::symlink_metadata(&blob_path)
                .expect("link remains")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).expect("target remains"), b"keep me");
    }

    fn set_modified(path: PathBuf, modified: SystemTime) {
        let file = fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .expect("open blob for timestamp update");
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .expect("set blob modification time");
    }
}
