//! The record a capture publishes beside the stream blocks it kept.
//!
//! The record binds the blocks to the call that produced them, with the
//! version, the exit code, and how complete the kept bytes are. A reader
//! authorizes a caller against it before handing back any stream block.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::block_ref::BLOCK_REF_KEY;

/// The key a bounded tool result names its capture under.
pub const CAPTURE_KEY: &str = "raw_capture";

/// The key inside a capture that holds the record block.
pub const RECORD_KEY: &str = "record";

/// The key a capture uses for the standard output block, which is also the
/// name that stream answers to in the catalog.
pub const STDOUT_KEY: &str = "stdout";

/// The key a capture uses for the standard error block, which is also the
/// name that stream answers to in the catalog.
pub const STDERR_KEY: &str = "stderr";

/// The key a failed capture names its refusal under, in place of a record.
pub const CAPTURE_FAILED_KEY: &str = "capture_failed";

/// The key a capture names the reason it stopped the command under.
pub const STOP_REASON_KEY: &str = "stop_reason";

/// The key a capture reports the bytes it read from standard output under.
pub const STDOUT_BYTES_KEY: &str = "stdout_bytes";

/// The key a capture reports the bytes it read from standard error under.
pub const STDERR_BYTES_KEY: &str = "stderr_bytes";

/// The record version this crate writes.
///
/// Bump it when a field changes meaning or when one of the enums below gains
/// a value. A reader on an older build refuses a newer record outright, so a
/// value it has never seen cannot make it guess at the rest of the record.
pub const CAPTURE_RECORD_VERSION: u64 = 1;

/// Why a capture stopped before the command's output ended.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStopReason {
    /// The capture reached the bytes it may hold. The producer is stopped
    /// rather than buffered without end.
    ByteBudget,
    /// The execution strategy ended the command at its wall budget before the
    /// command finished, so the kept bytes are the prefix it printed in time.
    WallBudget,
}

impl fmt::Display for CaptureStopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteBudget => write!(f, "capture byte budget reached"),
            Self::WallBudget => write!(f, "wall budget reached"),
        }
    }
}

/// How complete the kept bytes are.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureCompleteness {
    /// Every byte the command printed before it ended was kept.
    Complete,
    /// The kept bytes are a prefix of what the command printed: it was
    /// stopped, its exit status was never observed, or the platform reported
    /// none. How much more it printed is unknown.
    Truncated,
}

/// How a stream block's bytes are to be read.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEncoding {
    /// The bytes exactly as the pipe delivered them, not interpreted as text.
    Raw,
}

/// The marker that names a block.
///
/// The field carries the key the reclaim pass walks for, so a record and a tool
/// result name a block the same way and one walk reaches both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMarker {
    /// The full address of the block.
    #[serde(rename = "block_ref")]
    pub hash: String,
}

impl BlockMarker {
    /// Name the block at this address.
    pub fn new(hash: impl Into<String>) -> Self {
        Self { hash: hash.into() }
    }
}

/// What one stream contributed to the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamRecord {
    /// The block holding this stream's bytes, absent when it printed nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockMarker>,
    /// Bytes kept from this stream.
    pub bytes: u64,
    /// How those bytes are to be read.
    pub encoding: StreamEncoding,
}

/// The record one capture publishes beside its stream blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureRecord {
    /// The format version this record was written under.
    pub version: u64,
    /// The session whose log holds the blocks.
    pub session: String,
    /// The call that produced the output.
    pub call: String,
    /// The tool that ran.
    pub tool: String,
    /// When the capture began, absent when the host clock reads before the
    /// epoch.
    #[serde(default)]
    pub created_ms: Option<u64>,
    /// The exit status the caller observed, absent when no status was
    /// observed: the command was stopped, or the platform reported none.
    /// Absence never means success.
    #[serde(default)]
    pub exit: Option<i32>,
    /// How complete the kept bytes are.
    pub completeness: CaptureCompleteness,
    /// What ended the command before its output ended, absent when it printed
    /// everything before finishing on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<CaptureStopReason>,
    /// What standard output contributed.
    pub stdout: StreamRecord,
    /// What standard error contributed.
    pub stderr: StreamRecord,
}

/// Why a stored record was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRecordError {
    /// The bytes are not JSON at all.
    Unreadable(String),
    /// The record was written under a newer format than this build knows.
    UnsupportedVersion(u64),
    /// The value carries no version this build can read, or it names a
    /// version its fields do not match.
    Malformed(String),
}

impl fmt::Display for CaptureRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(detail) => write!(f, "record could not be read: {detail}"),
            Self::UnsupportedVersion(version) => write!(
                f,
                "record version {version} is newer than {CAPTURE_RECORD_VERSION}"
            ),
            Self::Malformed(detail) => write!(f, "record does not match its version: {detail}"),
        }
    }
}

impl Error for CaptureRecordError {}

/// Read a stored record, refusing one written under a newer format.
///
/// The version is read from the raw value before the typed read, so a record
/// from a newer capture format is refused as a version this build does not
/// know rather than reported as a broken record. A caller checks this before
/// trusting any field: a value added after this build cannot be told apart
/// from a corrupt one once the typed read has already failed.
pub fn read_capture_record(bytes: &[u8]) -> Result<CaptureRecord, CaptureRecordError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| CaptureRecordError::Unreadable(e.to_string()))?;
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| CaptureRecordError::Malformed("no version".into()))?;
    if version > CAPTURE_RECORD_VERSION {
        return Err(CaptureRecordError::UnsupportedVersion(version));
    }
    serde_json::from_value(value).map_err(|e| CaptureRecordError::Malformed(e.to_string()))
}

/// The capture a bounded tool result carries, read from the result value.
///
/// The result states the shape of its capture — which blocks were published,
/// how many raw bytes each stream kept, and what stopped the command — without
/// carrying the record body, which is a block of its own. A reader that holds
/// only the log therefore learns what the capture did and not what it
/// contained.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureReference {
    /// The block holding the capture record.
    pub record: Option<String>,
    /// The block holding each stream's raw bytes.
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Raw bytes kept from each stream.
    pub stdout_bytes: Option<u64>,
    pub stderr_bytes: Option<u64>,
    /// What stopped the command before its output ended.
    pub stop: Option<CaptureStopReason>,
    /// Whether the capture refused to publish the raw output, leaving only the
    /// bounded result.
    pub failed: bool,
}

/// Read the capture a tool result carries, or None when it carries none: an
/// output the model view held whole has nothing kept beside it.
pub fn read_capture_reference(value: &Value) -> Option<CaptureReference> {
    let reference = value.get(CAPTURE_KEY)?;
    Some(CaptureReference {
        record: marker_hash(reference.get(RECORD_KEY)),
        stdout: marker_hash(reference.get(STDOUT_KEY)),
        stderr: marker_hash(reference.get(STDERR_KEY)),
        stdout_bytes: reference.get(STDOUT_BYTES_KEY).and_then(Value::as_u64),
        stderr_bytes: reference.get(STDERR_BYTES_KEY).and_then(Value::as_u64),
        stop: reference
            .get(STOP_REASON_KEY)
            .and_then(|reason| serde_json::from_value(reason.clone()).ok()),
        failed: reference.get(CAPTURE_FAILED_KEY).is_some(),
    })
}

/// The address a block marker carries. A marker that names no address, or
/// names one that is not text, contributes nothing.
fn marker_hash(marker: Option<&Value>) -> Option<String> {
    let address = marker?.get(BLOCK_REF_KEY)?.as_str()?;
    Some(address.to_owned())
}

#[cfg(test)]
mod tests;
