//! Reading a session's recorded writes back, and undoing one when the target
//! still holds what the write left.
//!
//! Recovery asks one question per recorded write: does the target hold the
//! bytes the write left, the bytes it found, or something else? The first two
//! are safe to act on. The third means a later writer owns the file, and the
//! earlier bytes go beside the target rather than over it.

use std::ffi::OsStr;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs::{Metadata, metadata, read, read_dir, remove_file};
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_context::session_class::{FILE_TRANSACTION_DIR, session_dir};
use houyicoder_context::{BlockHash, ContextError, SandboxError, SessionId};

use super::record::{
    CommittedRecord, ImageRecord, PreparedRecord, RecordError, read_committed, read_prepared,
    write_committed,
};
use super::{FileTransaction, identity_of, image_of, mode_of, now_millis};

/// What a recorded write's target holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteState {
    /// The target holds the bytes the write left. The write landed, unless
    /// something put those bytes back afterwards.
    Applied,
    /// The target holds the bytes the write found. The write never landed,
    /// unless something undid it afterwards.
    NotApplied,
    /// Neither image, or an image whose file has since been replaced or given
    /// other permissions: something touched the target after the write.
    Conflict,
    /// The earlier bytes were never kept, so the target cannot be matched
    /// against them.
    Unverified,
}

/// One write as recovery found it.
#[derive(Clone, Debug)]
pub struct RecoveredWrite {
    /// The name the write recorded itself under.
    pub operation: String,
    /// The workspace path the write targeted.
    pub target: String,
    /// What the target holds now.
    pub state: WriteState,
    /// Whether the write has a committed record. A write that landed without
    /// one is given it while recovering, so this is true for every write the
    /// target still holds.
    pub committed: bool,
    /// When the committed record was published, in milliseconds since the
    /// epoch. Absent for a write that never committed and never landed.
    pub written_ms: Option<u64>,
    /// What the target held before the write.
    pub before: ImageSummary,
    /// What the write left: the committed record's image when it has one, the
    /// image the write expected to leave otherwise.
    pub after: ImageSummary,
}

/// One side of a recorded write, for a reader that reports what the record
/// kept without opening the stored bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageSummary {
    /// Whether the file was there at all.
    pub present: bool,
    /// The name the stored bytes are kept under. Absent when they were not
    /// kept: the file was not there, or it was over the backup ceiling.
    pub hash: Option<String>,
    /// How many bytes the file held.
    pub bytes: Option<u64>,
}

/// Every write a session recorded, split by whether recovery could judge it.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// The writes recovery could judge, in the order they were recorded.
    pub writes: Vec<RecoveredWrite>,
    /// The writes recovery could not judge, and why. One unreadable record
    /// does not take the rest of the list with it.
    pub unreadable: Vec<UnreadableWrite>,
}

/// One recorded write recovery could not judge.
#[derive(Debug)]
pub struct UnreadableWrite {
    /// The name the write recorded itself under, when its directory name
    /// could be read as text.
    pub operation: Option<String>,
    /// Why the write could not be judged.
    pub error: RecoveryError,
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The target already held the bytes the write found.
    Unchanged,
    /// The bytes the write found are back in the target.
    Restored,
    /// The target changed after the write, so the earlier bytes went to a
    /// copy beside it. No copy when the write created the file: there were
    /// no earlier bytes to keep.
    Kept { copy: Option<PathBuf> },
    /// The earlier bytes were never kept, so the write cannot be undone.
    Unbacked,
}

/// Why a recovery could not finish.
#[derive(Debug)]
pub enum RecoveryError {
    /// The session's write directories could not be listed.
    List { path: PathBuf, source: IoError },
    /// A record could not be read or understood.
    Record { path: PathBuf, reason: String },
    /// The target path could not be resolved.
    Target { path: String, error: SandboxError },
    /// The target could not be read.
    Read { path: PathBuf, source: IoError },
    /// A stored image could not be read back.
    Image { hash: String, error: ContextError },
    /// The session holds no write of that name.
    Unknown { operation: String },
    /// The earlier bytes could not be written back.
    Restore { path: String, error: SandboxError },
    /// The file the write created could not be removed.
    Remove { path: PathBuf, source: IoError },
}

impl Display for RecoveryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::List { path, source } => {
                write!(f, "list the recorded writes {}: {source}", path.display())
            }
            Self::Record { path, reason } => {
                write!(f, "read the record {}: {reason}", path.display())
            }
            Self::Target { path, error } => write!(f, "resolve the target {path}: {error}"),
            Self::Read { path, source } => {
                write!(f, "read the target {}: {source}", path.display())
            }
            Self::Image { hash, error } => write!(f, "read the stored image {hash}: {error}"),
            Self::Unknown { operation } => write!(f, "no recorded write named {operation}"),
            Self::Restore { path, error } => write!(f, "restore {path}: {error}"),
            Self::Remove { path, source } => {
                write!(f, "remove {}: {source}", path.display())
            }
        }
    }
}

impl FileTransaction {
    /// Every write the session recorded, with the state its target is in.
    ///
    /// A write that landed without its committed record is settled here: its
    /// committed record is published from what is on disk, so the next reader
    /// does not have to derive it again.
    ///
    /// A write whose record cannot be read or whose target cannot be resolved
    /// is reported beside the writes that could be judged, not in place of
    /// them: a listing that fails whole would hide every write behind one bad
    /// record, and the readable ones are exactly what a reader can act on.
    pub async fn recover(
        &self,
        session: SessionId,
        sandbox: &dyn SandboxSession,
    ) -> Result<RecoveryReport, RecoveryError> {
        let mut report = RecoveryReport::default();
        let Some(root) = self.root.as_deref() else {
            return Ok(report);
        };
        let dir = session_dir(root, session).join(FILE_TRANSACTION_DIR);
        let entries = match read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(report),
            Err(source) => return Err(RecoveryError::List { path: dir, source }),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| RecoveryError::List {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            match metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => continue,
                Err(source) if source.kind() == ErrorKind::NotFound => continue,
                Err(source) => {
                    report.unreadable.push(UnreadableWrite {
                        operation: None,
                        error: RecoveryError::List { path, source },
                    });
                    continue;
                }
            }
            paths.push(path);
        }
        paths.sort();
        for path in paths {
            // A directory whose name is not text was not named by this
            // journal, so it is not a write of this session.
            let Some(operation) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            match self.judge(operation, &path, sandbox).await {
                Ok(Some(write)) => report.writes.push(write),
                Ok(None) => {}
                Err(error) => report.unreadable.push(UnreadableWrite {
                    operation: Some(operation.to_string()),
                    error,
                }),
            }
        }
        Ok(report)
    }

    /// Judge one recorded write. Returns None when it never published its
    /// prepared record: it never reached the point of touching the target.
    async fn judge(
        &self,
        operation: &str,
        dir: &Path,
        sandbox: &dyn SandboxSession,
    ) -> Result<Option<RecoveredWrite>, RecoveryError> {
        let Some(prepared) = read_prepared(dir).map_err(record_error)? else {
            return Ok(None);
        };
        let target =
            sandbox
                .resolve_write(&prepared.target)
                .map_err(|error| RecoveryError::Target {
                    path: prepared.target.clone(),
                    error,
                })?;
        // The same lock the write path takes, held across the read and the
        // settle: a write landing in between would otherwise be recorded
        // against metadata that belongs to a different write.
        let lock = self.target_lock(&target);
        let _held = lock.lock().await;
        let committed = read_committed(dir).map_err(record_error)?;
        let mut written_ms = committed.as_ref().map(|record| record.written_ms);
        let mut has_commit = committed.is_some();
        let state = self
            .classify(&prepared, committed.as_ref(), &target)
            .await?;
        if state == WriteState::Applied && !has_commit {
            written_ms = Some(self.settle(dir, &prepared, &target)?);
            has_commit = true;
        }
        let after = committed
            .as_ref()
            .map_or(&prepared.expected_after, |record| &record.after);
        Ok(Some(RecoveredWrite {
            operation: operation.to_string(),
            target: prepared.target,
            state,
            committed: has_commit,
            written_ms,
            before: summary_of(&prepared.before),
            after: summary_of(after),
        }))
    }

    /// Undo one recorded write, if the target still holds what it left.
    ///
    /// The target is re-read under the same lock the write path takes, so a
    /// write this process ran in between cannot be undone by mistake. A file
    /// changed outside this process still can: the lock only orders the
    /// writers that take it.
    pub async fn restore(
        &self,
        session: SessionId,
        sandbox: &dyn SandboxSession,
        operation: &str,
    ) -> Result<RestoreOutcome, RecoveryError> {
        let Some(root) = self.root.as_deref() else {
            return Err(RecoveryError::Unknown {
                operation: operation.to_string(),
            });
        };
        // The name arrives from the guest and is joined into a host path, so
        // it has to be a name: an absolute string would replace the journal
        // directory and a parent component would climb out of it.
        if !is_one_component(operation) {
            return Err(RecoveryError::Unknown {
                operation: operation.to_string(),
            });
        }
        let dir = session_dir(root, session)
            .join(FILE_TRANSACTION_DIR)
            .join(operation);
        let prepared =
            read_prepared(&dir)
                .map_err(record_error)?
                .ok_or_else(|| RecoveryError::Unknown {
                    operation: operation.to_string(),
                })?;
        let target =
            sandbox
                .resolve_write(&prepared.target)
                .map_err(|error| RecoveryError::Target {
                    path: prepared.target.clone(),
                    error,
                })?;
        let lock = self.target_lock(&target);
        let _held = lock.lock().await;
        let committed = read_committed(&dir).map_err(record_error)?;
        match self
            .classify(&prepared, committed.as_ref(), &target)
            .await?
        {
            WriteState::NotApplied => Ok(RestoreOutcome::Unchanged),
            WriteState::Unverified => Ok(RestoreOutcome::Unbacked),
            WriteState::Conflict => self.keep_earlier(&prepared, operation, sandbox).await,
            WriteState::Applied => self.put_earlier(&prepared, &target, sandbox).await,
        }
    }

    /// Whether the target holds the bytes of one side of the write.
    async fn holds(&self, image: &ImageRecord, bytes: &[u8]) -> Result<bool, RecoveryError> {
        let Some(hash) = image.block_ref.as_deref() else {
            return Ok(false);
        };
        let stored = self
            .backend
            .block_get(&BlockHash(hash.to_string()))
            .await
            .map_err(|error| RecoveryError::Image {
                hash: hash.to_string(),
                error,
            })?;
        Ok(stored == bytes)
    }

    /// Which side of the write the target is on.
    ///
    /// A committed record names the file the write actually left, so the
    /// applied side is matched against it; the prepared record only states
    /// what the write expected to leave before it ran. Both sides are matched
    /// on the file's identity and permissions as well as its bytes: the same
    /// bytes in a file that was replaced or re-permissioned are not the file
    /// this write produced.
    async fn classify(
        &self,
        prepared: &PreparedRecord,
        committed: Option<&CommittedRecord>,
        target: &Path,
    ) -> Result<WriteState, RecoveryError> {
        let meta = match metadata(target) {
            Ok(meta) => meta,
            Err(source) if source.kind() == ErrorKind::NotFound => {
                return Ok(if prepared.before.present {
                    WriteState::Conflict
                } else {
                    WriteState::NotApplied
                });
            }
            Err(source) => {
                return Err(RecoveryError::Read {
                    path: target.to_path_buf(),
                    source,
                });
            }
        };
        let current = read(target).map_err(|source| RecoveryError::Read {
            path: target.to_path_buf(),
            source,
        })?;
        let left = committed.map_or(&prepared.expected_after, |record| &record.after);
        if !metadata_changed(left, Some(&meta)) && self.holds(left, &current).await? {
            return Ok(WriteState::Applied);
        }
        if !metadata_changed(&prepared.before, Some(&meta))
            && self.holds(&prepared.before, &current).await?
        {
            return Ok(WriteState::NotApplied);
        }
        let earlier_unkept = prepared.before.present
            && prepared.before.block_ref.is_none()
            && prepared.before.bytes == Some(current.len() as u64);
        if earlier_unkept {
            return Ok(WriteState::Unverified);
        }
        Ok(WriteState::Conflict)
    }

    /// Publish the committed record of a write that landed, using what is on
    /// disk now. Returns when the record states the write ran.
    fn settle(
        &self,
        dir: &Path,
        prepared: &PreparedRecord,
        target: &Path,
    ) -> Result<u64, RecoveryError> {
        let block_ref = prepared.expected_after.block_ref.clone();
        let after = match metadata(target) {
            Ok(meta) => image_of(&meta, block_ref),
            Err(_) => prepared.expected_after.clone(),
        };
        let written_ms = now_millis();
        let record = CommittedRecord::new(&prepared.operation, &prepared.target, after, written_ms);
        write_committed(dir, &record).map_err(record_error)?;
        Ok(written_ms)
    }

    /// Put the earlier bytes back over a target that still holds the write.
    async fn put_earlier(
        &self,
        prepared: &PreparedRecord,
        target: &Path,
        sandbox: &dyn SandboxSession,
    ) -> Result<RestoreOutcome, RecoveryError> {
        if !prepared.before.present {
            return match remove_file(target) {
                Ok(()) => Ok(RestoreOutcome::Restored),
                Err(source) if source.kind() == ErrorKind::NotFound => Ok(RestoreOutcome::Restored),
                Err(source) => Err(RecoveryError::Remove {
                    path: target.to_path_buf(),
                    source,
                }),
            };
        }
        let Some(hash) = prepared.before.block_ref.as_deref() else {
            return Ok(RestoreOutcome::Unbacked);
        };
        let bytes = self
            .backend
            .block_get(&BlockHash(hash.to_string()))
            .await
            .map_err(|error| RecoveryError::Image {
                hash: hash.to_string(),
                error,
            })?;
        sandbox
            .write_file(&prepared.target, bytes)
            .await
            .map_err(|error| RecoveryError::Restore {
                path: prepared.target.clone(),
                error,
            })?;
        Ok(RestoreOutcome::Restored)
    }

    /// Keep the earlier bytes beside a target that a later writer owns.
    async fn keep_earlier(
        &self,
        prepared: &PreparedRecord,
        operation: &str,
        sandbox: &dyn SandboxSession,
    ) -> Result<RestoreOutcome, RecoveryError> {
        if !prepared.before.present {
            return Ok(RestoreOutcome::Kept { copy: None });
        }
        let Some(hash) = prepared.before.block_ref.as_deref() else {
            return Ok(RestoreOutcome::Unbacked);
        };
        let bytes = self
            .backend
            .block_get(&BlockHash(hash.to_string()))
            .await
            .map_err(|error| RecoveryError::Image {
                hash: hash.to_string(),
                error,
            })?;
        let copy = format!("{}.recovery-{operation}", prepared.target);
        sandbox
            .write_file(&copy, bytes)
            .await
            .map_err(|error| RecoveryError::Restore {
                path: copy.clone(),
                error,
            })?;
        Ok(RestoreOutcome::Kept {
            copy: Some(PathBuf::from(copy)),
        })
    }
}

/// Whether a name is one ordinary path component. A journal entry is a
/// directory named by the write it records, so a name with a root, a parent
/// step, or a separator is not a journal entry and cannot be joined into one.
fn is_one_component(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(part)), None) => part == OsStr::new(name),
        _ => false,
    }
}

/// Whether the target is a different file from the one the record names, or
/// the same file under other permissions. A record that states neither says
/// nothing about it, and a recovery only compares what it can see.
fn metadata_changed(image: &ImageRecord, current: Option<&Metadata>) -> bool {
    let Some(current) = current else {
        return false;
    };
    let replaced = match (image.identity, identity_of(current)) {
        (Some(recorded), Some(now)) => recorded != now,
        _ => false,
    };
    let permissions = match (image.mode, mode_of(current)) {
        (Some(recorded), Some(now)) => recorded != now,
        _ => false,
    };
    replaced || permissions
}

/// What one side of a record states, without opening the stored bytes.
fn summary_of(image: &ImageRecord) -> ImageSummary {
    ImageSummary {
        present: image.present,
        hash: image.block_ref.clone(),
        bytes: image.bytes,
    }
}

/// A record failure with the file it names.
fn record_error(error: RecordError) -> RecoveryError {
    RecoveryError::Record {
        path: error.path().to_path_buf(),
        reason: error.reason(),
    }
}

#[cfg(test)]
mod tests;
