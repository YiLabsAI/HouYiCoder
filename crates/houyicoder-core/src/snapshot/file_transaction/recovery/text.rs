//! One recorded write's two sides, as text, for its detail to compare.
//!
//! The listing judges a recorded write without opening the bytes it kept, and
//! stays cheap however many writes a session made. This module opens those
//! bytes for the one row the user asked about, and holds the answer to the
//! room the detail has for it.

use std::str::from_utf8;

use houyicoder_context::session_class::{FILE_TRANSACTION_DIR, session_dir};
use houyicoder_context::{BlockHash, SessionId};

use super::super::record::{ImageRecord, read_committed, read_prepared};
use super::{FileTransaction, RecoveryError, is_one_component, record_error};

/// How many bytes of one side the detail shows. A larger file is cut here
/// rather than carried to the terminal whole.
const TEXT_ROOM_BYTES: usize = 16 * 1024;

/// How many lines of one side the detail shows, for a file whose lines are
/// short enough to fit the byte room many times over.
const TEXT_ROOM_LINES: usize = 400;

/// One side of a recorded write, as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SideText {
    /// The bytes as text, within the room the detail has.
    Text {
        /// The bytes as text.
        text: String,
        /// Whether more bytes were held back.
        truncated: bool,
    },
    /// The file was not there on this side of the write.
    Absent,
    /// The record kept no bytes for this side.
    NotKept,
    /// The bytes are not text, so there is nothing to set side by side.
    NotText,
}

/// Both sides of one recorded write, in the order the write went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteText {
    /// The name the write recorded itself under.
    pub operation: String,
    /// The workspace path the write targeted.
    pub target: String,
    /// What the target held before the write.
    pub before: SideText,
    /// What the write left.
    pub after: SideText,
}

impl FileTransaction {
    /// Both sides of one recorded write, as text, for its detail to compare.
    ///
    /// A side the record kept no bytes for, and one whose bytes are not text,
    /// are named as such rather than shown as a comparison of replacement
    /// characters.
    pub async fn text(
        &self,
        session: SessionId,
        operation: &str,
    ) -> Result<WriteText, RecoveryError> {
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
        let committed = read_committed(&dir).map_err(record_error)?;
        let after = committed
            .as_ref()
            .map_or(&prepared.expected_after, |record| &record.after);
        Ok(WriteText {
            operation: prepared.operation.clone(),
            target: prepared.target.clone(),
            before: self.side_text(&prepared.before).await?,
            after: self.side_text(after).await?,
        })
    }

    /// One recorded side, as text.
    async fn side_text(&self, image: &ImageRecord) -> Result<SideText, RecoveryError> {
        if !image.present {
            return Ok(SideText::Absent);
        }
        let Some(hash) = image.block_ref.as_deref() else {
            return Ok(SideText::NotKept);
        };
        let bytes = self
            .backend
            .block_get(&BlockHash(hash.to_string()))
            .await
            .map_err(|error| RecoveryError::Image {
                hash: hash.to_string(),
                error,
            })?;
        Ok(held_text(&bytes))
    }
}

/// The bytes as text, within the room the detail has.
fn held_text(bytes: &[u8]) -> SideText {
    let Ok(text) = from_utf8(bytes) else {
        return SideText::NotText;
    };
    if text.contains('\0') {
        return SideText::NotText;
    }
    let mut end = text.len();
    let mut truncated = false;
    if end > TEXT_ROOM_BYTES {
        end = text.floor_char_boundary(TEXT_ROOM_BYTES);
        truncated = true;
    }
    let mut shown = String::new();
    for (index, line) in text[..end].split_inclusive('\n').enumerate() {
        if index == TEXT_ROOM_LINES {
            truncated = true;
            break;
        }
        shown.push_str(line);
    }
    SideText::Text {
        text: shown,
        truncated,
    }
}

#[cfg(test)]
mod tests;
