//! The durable record a file write leaves behind.
//!
//! The prepared record is published before the target is touched and the
//! committed record after the write returns, so a crash leaves three
//! distinguishable states: nothing written, written without a commit, and
//! committed.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs::create_dir_all;
use std::io::Error as IoError;
use std::path::{Path, PathBuf};

use houyicoder_context::{WriteError, write_atomic};
use serde::Serialize;

/// The shape of a write record. A reader reads exactly this version.
const TRANSACTION_SCHEMA: u32 = 1;

/// The record published before the target is written.
pub(super) const PREPARED_FILE: &str = "prepared.json";

/// The record published once the target holds the new bytes.
pub(super) const COMMITTED_FILE: &str = "committed.json";

/// One side of a write: whether a file was there, and which bytes it held.
///
/// A present side with no stored bytes is a target too large to keep, which
/// keeps that limit distinguishable from an absent file.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ImageRecord {
    pub(super) present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) block_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) identity: Option<Identity>,
}

impl ImageRecord {
    /// Nothing was there, so a recovery removes whatever the write created.
    pub(super) fn absent() -> Self {
        Self {
            present: false,
            block_ref: None,
            bytes: None,
            mode: None,
            identity: None,
        }
    }
}

/// What identifies one file on disk, so a recovery can tell the file it
/// recorded from a different one now sitting at the same path.
#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct Identity {
    pub(super) device: u64,
    pub(super) inode: u64,
}

/// The record published before the write runs.
#[derive(Debug, Serialize)]
pub(super) struct PreparedRecord {
    pub(super) schema: u32,
    pub(super) operation: String,
    pub(super) session: String,
    pub(super) target: String,
    pub(super) before: ImageRecord,
    pub(super) expected_after: ImageRecord,
}

impl PreparedRecord {
    pub(super) fn new(
        operation: &str,
        session: &str,
        target: &str,
        before: ImageRecord,
        expected_after: ImageRecord,
    ) -> Self {
        Self {
            schema: TRANSACTION_SCHEMA,
            operation: operation.to_string(),
            session: session.to_string(),
            target: target.to_string(),
            before,
            expected_after,
        }
    }
}

/// The record published after the write ran.
#[derive(Debug, Serialize)]
pub(super) struct CommittedRecord {
    pub(super) schema: u32,
    pub(super) operation: String,
    pub(super) target: String,
    pub(super) after: ImageRecord,
    pub(super) written_ms: u64,
}

impl CommittedRecord {
    pub(super) fn new(operation: &str, target: &str, after: ImageRecord, written_ms: u64) -> Self {
        Self {
            schema: TRANSACTION_SCHEMA,
            operation: operation.to_string(),
            target: target.to_string(),
            after,
            written_ms,
        }
    }
}

/// Why a record could not be published.
#[derive(Debug)]
pub(super) enum RecordError {
    /// The operation's directory could not be created.
    Directory { path: PathBuf, source: IoError },
    /// The record could not be encoded.
    Encode {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The record could not be written to disk.
    Publish { path: PathBuf, error: WriteError },
}

impl RecordError {
    /// The file the record was being written to.
    pub(super) fn path(&self) -> &Path {
        match self {
            Self::Directory { path, .. }
            | Self::Encode { path, .. }
            | Self::Publish { path, .. } => path,
        }
    }
}

impl Display for RecordError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Directory { path, source } => {
                write!(
                    f,
                    "create the record directory {}: {source}",
                    path.display()
                )
            }
            Self::Encode { path, source } => {
                write!(f, "encode the record {}: {source}", path.display())
            }
            Self::Publish { path, error } => {
                write!(f, "publish the record {}: {error}", path.display())
            }
        }
    }
}

/// Publish the prepared record, creating the operation's directory on the
/// way. A failure here means the target was never touched.
pub(super) fn write_prepared(dir: &Path, record: &PreparedRecord) -> Result<(), RecordError> {
    publish_record(dir, PREPARED_FILE, record)
}

/// Publish the committed record. A failure here leaves the prepared record
/// alone on disk, which reads as a write that ran without a commit.
pub(super) fn write_committed(dir: &Path, record: &CommittedRecord) -> Result<(), RecordError> {
    publish_record(dir, COMMITTED_FILE, record)
}

/// Create the directory, encode, publish whole.
fn publish_record<T: Serialize>(dir: &Path, name: &str, record: &T) -> Result<(), RecordError> {
    let path = dir.join(name);
    create_dir_all(dir).map_err(|source| RecordError::Directory {
        path: dir.to_path_buf(),
        source,
    })?;
    let body = serde_json::to_vec(record).map_err(|source| RecordError::Encode {
        path: path.clone(),
        source,
    })?;
    match write_atomic(&path, &body) {
        Ok(()) => Ok(()),
        // The target holds the bytes; only their survival across a crash is
        // weaker, and the caller must not undo work whose record is on disk.
        Err(error) if error.published() => {
            tracing::warn!(path = %path.display(), %error, "record not flushed");
            Ok(())
        }
        Err(error) => Err(RecordError::Publish { path, error }),
    }
}
