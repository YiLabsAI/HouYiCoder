//! The content-addressed block store: the bytes an event references by hash
//! instead of carrying inline.
//!
//! Blocks live under a dot-cas directory at the root and are shared across
//! sessions, because content addressing dedups them wherever the same bytes
//! appear. Each block is sharded by the first two hex characters of its hash
//! so no one directory grows without bound. A write goes to a temp sibling,
//! fsyncs, then renames, so a reader sees a whole block or nothing at all.
//!
//! Reclaim is a maintenance pass rather than a hot path: it owns its lock and
//! budget, marks the blocks the live references name, and frees what none of
//! them does. The pass lives in the gc submodule.

use std::fs;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

#[cfg(unix)]
use std::{fs::File, os::unix::fs::OpenOptionsExt};

use houyicoder_context::{BlockHash, ContextError};

use crate::sha256_file;
use crate::sha256_hex;

use crate::local_file::LocalFileBackend;

pub use gc::{ReclaimError, ReclaimPolicy, ReclaimReport, ReclaimStop};

/// Makes each writer's temporary file name unique within its process. Two
/// concurrent writers of the same block must not share a temp path: both
/// would truncate the same file, and the first rename could publish a
/// half-written block.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many hex characters name a shard directory.
pub(super) const SHARD_CHARS: usize = 2;
/// The suffix of a stored block.
pub(super) const BLOCK_SUFFIX: &str = ".bin";
/// The suffix of a block write that never reached its rename.
pub(super) const TEMP_SUFFIX: &str = ".bin.tmp";

/// The temp sibling one writer fills before renaming it onto the block's
/// path. The process id separates writers in different processes and the
/// counter separates writers within one.
fn temp_block_path(block_path: &Path, hash: &BlockHash) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    block_path.with_file_name(format!("{}.{}.{sequence}{TEMP_SUFFIX}", hash.0, id()))
}

impl LocalFileBackend {
    fn cas_dir(&self) -> PathBuf {
        self.root.join(".cas")
    }

    fn block_path(&self, hash: &BlockHash) -> PathBuf {
        let prefix = hash.0.get(..SHARD_CHARS).unwrap_or("");
        self.cas_dir()
            .join(prefix)
            .join(format!("{}{BLOCK_SUFFIX}", hash.0))
    }

    pub(super) fn block_put_sync(&self, block: Vec<u8>) -> Result<BlockHash, ContextError> {
        let hash = sha256_hex(&block);
        let path = self.block_path(&hash);
        // Dedup: the same content is already on disk, so there is nothing to
        // write -- unless the file there does not hold these bytes. A block
        // damaged after its write must be rewritten here: reads verify the
        // address, so stopping early would report success for a block every
        // later read rejects.
        if path.exists() && sha256_file(&path).is_ok_and(|stored| stored == hash) {
            // A hit is a fresh claim on bytes an earlier event may still be
            // about to name. Refresh their age, so a reclaim pass running
            // between this put and the append does not free them. A touch
            // that fails falls through to the rewrite below, which publishes
            // a new time anyway.
            if self.touch_block(&path).is_ok() {
                return Ok(hash);
            }
        }
        if let Some(parent) = path.parent() {
            Self::ensure_dir(parent)?;
        }
        // Atomic write to a temp sibling + fsync + rename so a crash never
        // leaves a partial block (a reader gets the complete block or nothing).
        // The temp name carries pid and a per-process sequence so concurrent
        // writers of one block cannot truncate each other's file.
        let tmp = temp_block_path(&path, &hash);
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&tmp).map_err(|_| ContextError::Io)?;
        f.write_all(&block).map_err(|_| ContextError::Io)?;
        f.sync_all().map_err(|_| ContextError::Io)?;
        drop(f);
        fs::rename(&tmp, &path).map_err(|_| ContextError::Io)?;
        // Fsync the parent directory so the rename survives a crash. This covers
        // the entry inside the shard, not a newly created shard itself, which
        // is the durability the rest of the store keeps. Windows cannot open
        // a directory for fsync, so there the rename rests on the filesystem
        // journal; a Unix failure is an error because the caller was told the
        // block is stored.
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            File::open(dir)
                .and_then(|handle| handle.sync_all())
                .map_err(|_| ContextError::Io)?;
        }
        Ok(hash)
    }

    /// Mark a stored block as just used. The reclaim pass keeps a block while
    /// its time is inside the write lease, so a put that finds the bytes
    /// already stored must refresh that time: the caller is about to append
    /// the event that names them.
    fn touch_block(&self, path: &Path) -> Result<(), ContextError> {
        let handle = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|_| ContextError::Io)?;
        handle
            .set_modified(SystemTime::now())
            .map_err(|_| ContextError::Io)
    }

    pub(super) fn block_get_sync(&self, hash: &BlockHash) -> Result<Vec<u8>, ContextError> {
        if !hash.is_canonical() {
            return Err(ContextError::Corrupt(format!(
                "block reference is not a sha-256 digest: {}",
                hash.0
            )));
        }
        let path = self.block_path(hash);
        match fs::read(&path) {
            Ok(bytes) => {
                // Content addressing is the storage contract: the bytes must
                // hash to the address they were found at. A tampered or
                // swapped block is corrupt, not a successful read.
                let actual = sha256_hex(&bytes);
                if actual != *hash {
                    return Err(ContextError::Corrupt(format!(
                        "block {} does not match its content hash {}",
                        hash.0, actual.0
                    )));
                }
                Ok(bytes)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Err(ContextError::NotFound),
            Err(_) => Err(ContextError::Io),
        }
    }
}

mod gc;

#[cfg(test)]
mod tests;
