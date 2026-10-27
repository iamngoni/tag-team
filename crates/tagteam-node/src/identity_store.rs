//! Private durable node identity storage.
//!
//! This module owns the one-file enrollment record; directory and file permissions are
//! tightened before use, and symlinked data paths are rejected to protect key material.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tagteam_pki::Invite;

use crate::{IdentityStore, IdentityStoreError, NodeIdentity};

const IDENTITY_FILE: &str = "identity.json";

#[derive(Debug, thiserror::Error)]
pub enum FileIdentityError {
    #[error("node data directory is unsafe or unavailable")]
    Directory,
    #[error("node identity file is unsafe or unavailable")]
    File,
    #[error("node identity file is invalid")]
    Invalid,
}

#[derive(Clone, Debug)]
pub struct FileIdentityStore {
    data_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct IdentityRecord {
    invite: String,
    address: String,
    fingerprint: String,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    node_id: String,
}

impl FileIdentityStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn default_data_dir() -> Result<PathBuf, FileIdentityError> {
        let home = std::env::var_os("HOME").ok_or(FileIdentityError::Directory)?;
        Ok(PathBuf::from(home).join(".local/share/tagteam-node"))
    }

    pub fn load(&self) -> Result<Option<(String, NodeIdentity)>, FileIdentityError> {
        self.ensure_directory()?;
        let path = self.data_dir.join(IDENTITY_FILE);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(FileIdentityError::File),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(FileIdentityError::File);
            }
            Ok(_metadata) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if _metadata.permissions().mode() & 0o777 != 0o600 {
                        return Err(FileIdentityError::File);
                    }
                }
            }
        }
        let bytes = fs::read(&path).map_err(|_| FileIdentityError::File)?;
        let record: IdentityRecord =
            serde_json::from_slice(&bytes).map_err(|_| FileIdentityError::Invalid)?;
        let invite = Invite::decode(&record.invite).map_err(|_| FileIdentityError::Invalid)?;
        let node_id = record
            .node_id
            .parse()
            .map_err(|_| FileIdentityError::Invalid)?;
        if record.address != invite.address || record.fingerprint != invite.hub_fingerprint.to_hex()
        {
            return Err(FileIdentityError::Invalid);
        }
        Ok(Some((
            record.invite.clone(),
            NodeIdentity {
                node_id,
                invite: record.invite.clone(),
                certificate_der: record.certificate_der,
                private_key_der: record.private_key_der,
                hub_address: record.address,
                hub_fingerprint: invite.hub_fingerprint,
            },
        )))
    }

    fn ensure_directory(&self) -> Result<(), FileIdentityError> {
        reject_path_symlinks(&self.data_dir)?;
        fs::create_dir_all(&self.data_dir).map_err(|_| FileIdentityError::Directory)?;
        let metadata =
            fs::symlink_metadata(&self.data_dir).map_err(|_| FileIdentityError::Directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(FileIdentityError::Directory);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.data_dir, fs::Permissions::from_mode(0o700))
                .map_err(|_| FileIdentityError::Directory)?;
        }
        Ok(())
    }
}

fn reject_path_symlinks(path: &Path) -> Result<(), FileIdentityError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(FileIdentityError::Directory),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(FileIdentityError::Directory),
        _ => Ok(()),
    }
}

impl IdentityStore for FileIdentityStore {
    fn persist(&self, identity: &NodeIdentity) -> Result<(), IdentityStoreError> {
        self.persist_with_invite(identity, &identity.invite)
            .map_err(|_| IdentityStoreError)
    }
}

impl FileIdentityStore {
    /// Persists the exact invite used for enrollment alongside all transport identity fields.
    pub fn persist_with_invite(
        &self,
        identity: &NodeIdentity,
        invite: &str,
    ) -> Result<(), FileIdentityError> {
        self.ensure_directory()?;
        let decoded = Invite::decode(invite).map_err(|_| FileIdentityError::Invalid)?;
        if decoded.address != identity.hub_address
            || decoded.hub_fingerprint != identity.hub_fingerprint
        {
            return Err(FileIdentityError::Invalid);
        }
        let record = IdentityRecord {
            invite: invite.to_owned(),
            address: identity.hub_address.clone(),
            fingerprint: identity.hub_fingerprint.to_hex(),
            certificate_der: identity.certificate_der.clone(),
            private_key_der: identity.private_key_der.clone(),
            node_id: identity.node_id.to_string(),
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| FileIdentityError::Invalid)?;
        let destination = self.data_dir.join(IDENTITY_FILE);
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(FileIdentityError::File);
        }
        let temporary = self
            .data_dir
            .join(format!(".identity-{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| FileIdentityError::File)?;
        let result = (|| {
            file.write_all(&bytes)
                .map_err(|_| FileIdentityError::File)?;
            file.sync_all().map_err(|_| FileIdentityError::File)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                    .map_err(|_| FileIdentityError::File)?;
            }
            fs::rename(&temporary, &destination).map_err(|_| FileIdentityError::File)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}
