//! Atomic content-addressed blob storage with verification on every read.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
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

    fn path_for(&self, reference: &BlobRef) -> PathBuf {
        let digest = reference.digest_hex();
        self.root.join(&digest[..2]).join(digest)
    }
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
}
