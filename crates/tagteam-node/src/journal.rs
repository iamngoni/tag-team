//! Crash durable node command receipts and completed results.
//!
//! Receipts are written before dispatch and protobuf results before acknowledgement; replayed
//! command IDs therefore return stored results or remain safely non-executable after a crash.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use prost::Message;
use serde::{Deserialize, Serialize};

type Ack = tagteam_proto::v1::CommandAck;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("command journal could not be read or written")]
    Io,
    #[error("command journal contains invalid data")]
    Invalid,
}

#[derive(Clone, Debug)]
pub struct CommandJournal {
    path: PathBuf,
    entries: std::sync::Arc<Mutex<BTreeMap<String, Entry>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    result: Option<String>,
    #[serde(default)]
    cancelled: bool,
}

#[derive(Serialize, Deserialize)]
struct JournalFile {
    version: u8,
    entries: BTreeMap<String, Entry>,
}

pub enum BeginCommand {
    Execute,
    InProgress,
    Completed(Ack),
}

impl CommandJournal {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, JournalError> {
        let path = path.into();
        ensure_private_parent(path.parent().ok_or(JournalError::Io)?)?;
        reject_symlink(&path)?;
        let entries = match fs::read(&path) {
            Ok(bytes) => {
                let file: JournalFile =
                    serde_json::from_slice(&bytes).map_err(|_| JournalError::Invalid)?;
                if file.version != 1 {
                    return Err(JournalError::Invalid);
                }
                for entry in file.entries.values() {
                    if let Some(encoded) = &entry.result {
                        let bytes = STANDARD
                            .decode(encoded)
                            .map_err(|_| JournalError::Invalid)?;
                        Ack::decode(bytes.as_slice()).map_err(|_| JournalError::Invalid)?;
                    }
                }
                file.entries
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(_) => return Err(JournalError::Io),
        };
        Ok(Self {
            path,
            entries: std::sync::Arc::new(Mutex::new(entries)),
        })
    }

    pub fn begin(&self, command_id: &str) -> Result<BeginCommand, JournalError> {
        let mut entries = self.entries.lock().map_err(|_| JournalError::Io)?;
        if let Some(entry) = entries.get(command_id) {
            if let Some(encoded) = &entry.result {
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| JournalError::Invalid)?;
                return Ok(BeginCommand::Completed(
                    Ack::decode(bytes.as_slice()).map_err(|_| JournalError::Invalid)?,
                ));
            }
            if !entry.cancelled {
                return Ok(BeginCommand::InProgress);
            }

            // The command has arrived after a durable cancellation. Persist the
            // terminal acknowledgement before returning it so a later replay
            // cannot start the operation.
            let ack = cancelled_ack(command_id);
            let mut updated = entries.clone();
            let Some(updated_entry) = updated.get_mut(command_id) else {
                return Err(JournalError::Invalid);
            };
            updated_entry.result = Some(STANDARD.encode(ack.encode_to_vec()));
            self.persist(&updated)?;
            *entries = updated;
            return Ok(BeginCommand::Completed(ack));
        }
        entries.insert(
            command_id.to_owned(),
            Entry {
                result: None,
                cancelled: false,
            },
        );
        if let Err(error) = self.persist(&entries) {
            entries.remove(command_id);
            return Err(error);
        }
        Ok(BeginCommand::Execute)
    }

    /// Records cancellation before or during a command. The marker is durable so
    /// a command replayed after a node restart still cannot perform side effects.
    pub fn cancel(&self, command_id: &str) -> Result<(), JournalError> {
        if command_id.is_empty() {
            return Err(JournalError::Invalid);
        }
        let mut entries = self.entries.lock().map_err(|_| JournalError::Io)?;
        let mut updated = entries.clone();
        match updated.get_mut(command_id) {
            Some(entry) if entry.result.is_some() || entry.cancelled => return Ok(()),
            Some(entry) => entry.cancelled = true,
            None => {
                updated.insert(
                    command_id.to_owned(),
                    Entry {
                        result: None,
                        cancelled: true,
                    },
                );
            }
        }
        self.persist(&updated)?;
        *entries = updated;
        Ok(())
    }

    pub fn complete(&self, ack: &Ack) -> Result<(), JournalError> {
        if ack.command_id.is_empty()
            || ack
                .result
                .as_ref()
                .is_none_or(|result| result.command_id != ack.command_id)
        {
            return Err(JournalError::Invalid);
        }
        let mut entries = self.entries.lock().map_err(|_| JournalError::Io)?;
        let Some(entry) = entries.get(&ack.command_id) else {
            return Err(JournalError::Invalid);
        };
        if entry.result.is_some() {
            return Err(JournalError::Invalid);
        }
        let mut updated = entries.clone();
        if let Some(entry) = updated.get_mut(&ack.command_id) {
            entry.result = Some(STANDARD.encode(ack.encode_to_vec()));
        }
        self.persist(&updated)?;
        *entries = updated;
        Ok(())
    }

    pub fn pending(&self) -> Result<Vec<Ack>, JournalError> {
        let entries = self.entries.lock().map_err(|_| JournalError::Io)?;
        entries
            .values()
            .filter_map(|entry| entry.result.as_ref())
            .map(|encoded| {
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| JournalError::Invalid)?;
                Ack::decode(bytes.as_slice()).map_err(|_| JournalError::Invalid)
            })
            .collect()
    }

    fn persist(&self, entries: &BTreeMap<String, Entry>) -> Result<(), JournalError> {
        reject_symlink(&self.path)?;
        let contents = serde_json::to_vec(&JournalFile {
            version: 1,
            entries: entries.clone(),
        })
        .map_err(|_| JournalError::Invalid)?;
        let parent = self.path.parent().ok_or(JournalError::Io)?;
        let temp = parent.join(format!(".node-journal-{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|_| JournalError::Io)?;
        let result = (|| {
            file.write_all(&contents).map_err(|_| JournalError::Io)?;
            file.sync_all().map_err(|_| JournalError::Io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
                    .map_err(|_| JournalError::Io)?;
            }
            fs::rename(&temp, &self.path).map_err(|_| JournalError::Io)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

fn cancelled_ack(command_id: &str) -> Ack {
    Ack {
        command_id: command_id.to_owned(),
        result: Some(tagteam_proto::v1::CommandResult {
            command_id: command_id.to_owned(),
            result: Some(tagteam_proto::v1::command_result::Result::Error(
                tagteam_proto::v1::Error {
                    code: tagteam_proto::v1::ErrorCode::Cancelled as i32,
                    message: "command was cancelled".into(),
                    retryable: false,
                },
            )),
        }),
    }
}

fn ensure_private_parent(path: &Path) -> Result<(), JournalError> {
    fs::create_dir_all(path).map_err(|_| JournalError::Io)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| JournalError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(JournalError::Io);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| JournalError::Io)?;
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), JournalError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(JournalError::Io);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(JournalError::Io);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str) -> Ack {
        Ack {
            command_id: id.to_owned(),
            result: Some(tagteam_proto::v1::CommandResult {
                command_id: id.to_owned(),
                result: Some(tagteam_proto::v1::command_result::Result::Done(
                    tagteam_proto::v1::Done { ok: true },
                )),
            }),
        }
    }

    #[test]
    fn duplicate_receipt_never_requests_execution_twice() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal =
            CommandJournal::open(directory.path().join("commands.json")).expect("journal opens");
        assert!(matches!(
            journal.begin("command-1"),
            Ok(BeginCommand::Execute)
        ));
        assert!(matches!(
            journal.begin("command-1"),
            Ok(BeginCommand::InProgress)
        ));
        journal
            .complete(&result("command-1"))
            .expect("result persists");
        assert!(matches!(
            journal.begin("command-1"),
            Ok(BeginCommand::Completed(_))
        ));
    }

    #[test]
    fn cancellation_before_dispatch_survives_store_reopen() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("commands.json");
        let journal = CommandJournal::open(&path).expect("journal opens");
        journal
            .cancel("cancelled-before-dispatch")
            .expect("cancellation persists");
        drop(journal);

        let reopened = CommandJournal::open(path).expect("journal reopens");
        let BeginCommand::Completed(ack) = reopened
            .begin("cancelled-before-dispatch")
            .expect("cancelled command receipt loads")
        else {
            panic!("a durable cancellation must never request execution");
        };
        assert!(matches!(
            ack.result.and_then(|result| result.result),
            Some(tagteam_proto::v1::command_result::Result::Error(error))
                if error.code == tagteam_proto::v1::ErrorCode::Cancelled as i32
        ));
        assert!(matches!(
            reopened.begin("cancelled-before-dispatch"),
            Ok(BeginCommand::Completed(_))
        ));
    }

    #[test]
    fn completed_result_survives_store_reopen() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("commands.json");
        let journal = CommandJournal::open(&path).expect("journal opens");
        assert!(matches!(
            journal.begin("command-2"),
            Ok(BeginCommand::Execute)
        ));
        let expected = result("command-2");
        journal.complete(&expected).expect("result persists");
        drop(journal);
        let reopened = CommandJournal::open(path).expect("journal reopens");
        let BeginCommand::Completed(actual) = reopened.begin("command-2").expect("receipt loads")
        else {
            panic!("completed command must replay its result");
        };
        assert_eq!(actual, expected);
    }
}
