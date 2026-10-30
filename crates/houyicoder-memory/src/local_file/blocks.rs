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
use std::io::Error as IoError;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::{BlockHash, BlockWindow, ContextError};

use crate::sha256_file;
use crate::sha256_hex;
use crate::sha256_reader;

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

    /// Read one window of a stored block without loading the rest of it.
    ///
    /// The address is checked before the window is returned, but checking it
    /// means hashing the whole block, which is the cost a windowed read exists
    /// to avoid paying per page. The result of that check is remembered
    /// against the file's size and modification time, so the first read of a
    /// block pays one pass and later windows of it pay only the seek and the
    /// window.
    ///
    /// A block file is published by rename under a name taken from its own
    /// content, so the bytes at one path do not change without a republish
    /// that also moves the modification time. The memory is an accelerator
    /// over that fact, not a guarantee: an edit that keeps both size and time
    /// the same would go unverified.
    pub(super) fn block_get_range_sync(
        &self,
        hash: &BlockHash,
        offset: u64,
        len: usize,
    ) -> Result<BlockWindow, ContextError> {
        if !hash.is_canonical() {
            return Err(ContextError::Corrupt(format!(
                "block reference is not a sha-256 digest: {}",
                hash.0
            )));
        }
        let path = self.block_path(hash);
        let mut file = File::open(&path).map_err(open_error)?;
        let meta = file.metadata().map_err(|_| ContextError::Io)?;
        let stamp = BlockStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        };
        self.verify_block(hash, &mut file, stamp)?;
        let total = stamp.len;
        let start = offset.min(total);
        let wanted = usize::try_from(total - start)
            .unwrap_or(usize::MAX)
            .min(len);
        file.seek(SeekFrom::Start(start))
            .map_err(|_| ContextError::Io)?;
        let mut bytes = vec![0u8; wanted];
        file.read_exact(&mut bytes).map_err(|_| ContextError::Io)?;
        Ok(BlockWindow { bytes, total })
    }

    /// Confirm the bytes behind this open handle hash to the address, unless
    /// this process already confirmed that identity. The handle is hashed in
    /// place and rewound, so a block replaced between the open and the check
    /// cannot be verified as one file and read as another.
    ///
    /// The lock covers the lookup and the insert, not the hash. Holding it
    /// across a whole-block hash would queue every other reader behind one
    /// first read; two readers of the same unverified block may instead each
    /// pay one pass, which is the same cost the memory exists to stop paying
    /// per window.
    fn verify_block(
        &self,
        hash: &BlockHash,
        file: &mut File,
        stamp: BlockStamp,
    ) -> Result<(), ContextError> {
        if self.remembered_block(hash, &stamp) {
            return Ok(());
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| ContextError::Io)?;
        let actual = sha256_reader(file).map_err(open_error)?;
        if actual != *hash {
            return Err(ContextError::Corrupt(format!(
                "block {} does not match its content hash {}",
                hash.0, actual.0
            )));
        }
        self.remember_block(hash, stamp);
        Ok(())
    }

    /// Whether this process already confirmed the block under this identity.
    fn remembered_block(&self, hash: &BlockHash, stamp: &BlockStamp) -> bool {
        let verified = self
            .verified_blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        verified.get(hash) == Some(stamp)
    }

    /// Record a block identity this process has confirmed.
    fn remember_block(&self, hash: &BlockHash, stamp: BlockStamp) {
        let mut verified = self
            .verified_blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The memory is a read accelerator, not a record: when it fills, it is
        // dropped whole rather than grown without end.
        if verified.len() >= VERIFIED_BLOCKS_REMEMBERED {
            verified.clear();
        }
        verified.insert(hash.clone(), stamp);
    }
}

/// How many verified block identities one backend remembers. Each entry is an
/// address plus a size and a time, so the ceiling is small in bytes; it exists
/// so a long process reading many blocks does not grow this map forever.
const VERIFIED_BLOCKS_REMEMBERED: usize = 4096;

/// The identity a block's verified bytes were seen under. A block is written
/// by atomic rename, so a rewrite publishes a new file with a new time, and a
/// new size when the content differs in length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BlockStamp {
    /// Stored length.
    len: u64,
    /// Last modification time, absent when the platform does not report one.
    modified: Option<SystemTime>,
}

/// Map a file open failure onto the storage error the caller understands.
fn open_error(error: IoError) -> ContextError {
    if error.kind() == ErrorKind::NotFound {
        ContextError::NotFound
    } else {
        ContextError::Io
    }
}

mod gc;

#[cfg(test)]
mod tests;
