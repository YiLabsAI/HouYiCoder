//! Publishing a small file that a later reader must never see half written:
//! the bytes go to a temporary file named for this writer, are flushed to the
//! device, and are renamed into place. A crash leaves either the previous
//! contents or the new ones, never a mixture of the two.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs::{File, remove_file, rename};
use std::io::{Error as IoError, Write};
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes two temporary files written in one process, so a second
/// writer cannot truncate the first one's half-written bytes.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The step of a durable write that failed, with the path it failed on.
///
/// The three steps are separate because they leave different debris behind:
/// a create failure writes nothing, a write failure leaves a temporary file
/// that this call removes, and a publish failure leaves the target at its
/// previous contents.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// The temporary file could not be created beside the target.
    Create {
        /// The temporary file that could not be created.
        path: PathBuf,
        /// The failure the operating system reported.
        source: IoError,
    },
    /// The bytes could not be written, or the file could not be flushed.
    Write {
        /// The temporary file holding the partial write.
        path: PathBuf,
        /// The failure the operating system reported.
        source: IoError,
    },
    /// The completed temporary file could not be renamed onto the target.
    Publish {
        /// The target the rename was aimed at.
        path: PathBuf,
        /// The failure the operating system reported.
        source: IoError,
    },
    /// The bytes are published, but flushing the directory that holds the new
    /// name failed, so a crash before the file system settles could still lose
    /// the rename.
    Sync {
        /// The target that already holds the new bytes.
        path: PathBuf,
        /// The failure the operating system reported.
        source: IoError,
    },
}

impl WriteError {
    /// The path this write failed on: the temporary file for the first two
    /// steps, the target for the last two.
    pub fn path(&self) -> &Path {
        match self {
            Self::Create { path, .. }
            | Self::Write { path, .. }
            | Self::Publish { path, .. }
            | Self::Sync { path, .. } => path,
        }
    }

    /// Whether the target already holds the new bytes when this error was
    /// returned. A caller must not undo work whose record is on disk.
    pub fn published(&self) -> bool {
        matches!(self, Self::Sync { .. })
    }
}

impl Display for WriteError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Create { path, source } => {
                write!(f, "create the temporary file {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(f, "write the temporary file {}: {source}", path.display())
            }
            Self::Publish { path, source } => {
                write!(f, "publish {} into place: {source}", path.display())
            }
            Self::Sync { path, source } => {
                write!(
                    f,
                    "flush the directory holding {}: {source}",
                    path.display()
                )
            }
        }
    }
}

/// Write bytes to a path by rename, so a reader sees the previous contents or
/// the new ones and nothing between them.
///
/// The temporary file carries this process's id and a sequence, so two
/// writers never share one and each can clean up only its own leftovers.
///
/// A failed directory flush is its own error, and its published() answers
/// true: the target already holds the new bytes, so whether that limit
/// matters is the caller's call.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.{}.{sequence}.tmp", id()));
    let mut file = File::create(&temp).map_err(|source| WriteError::Create {
        path: temp.clone(),
        source,
    })?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(source) = written {
        drop(remove_file(&temp));
        return Err(WriteError::Write { path: temp, source });
    }
    rename(&temp, path).map_err(|source| {
        drop(remove_file(&temp));
        WriteError::Publish {
            path: path.to_path_buf(),
            source,
        }
    })?;
    sync_dir(parent).map_err(|source| WriteError::Sync {
        path: path.to_path_buf(),
        source,
    })
}

/// Make a completed rename durable by flushing the directory that holds it.
/// Windows cannot open a directory for flushing this way; there the rename is
/// left to the file system to settle.
#[cfg(unix)]
fn sync_dir(parent: &Path) -> Result<(), IoError> {
    File::open(parent).and_then(|dir| dir.sync_all())
}

#[cfg(not(unix))]
fn sync_dir(_parent: &Path) -> Result<(), IoError> {
    Ok(())
}

#[cfg(test)]
mod tests;
