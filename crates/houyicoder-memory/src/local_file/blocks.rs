//! The content-addressed block store: the bytes an event references by hash
//! instead of carrying inline.
//!
//! Blocks live under a dot-cas directory at the root and are shared across
//! sessions, because content addressing dedups them wherever the same bytes
//! appear. Each block is sharded by the first two hex characters of its hash
//! so no one directory grows without bound. A write goes to a temp sibling,
//! fsyncs, then renames, so a reader sees a whole block or nothing at all.
//!
//! Reclaim is a maintenance pass rather than a hot path: it reads every
//! session log for the block references it holds and deletes the blocks none
//! of them name.

use std::collections::HashSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::{fs::File, os::unix::fs::OpenOptionsExt};

use houyicoder_context::{BlockHash, ContextError, session_class::LOG_FILE};

use crate::sha256_file;
use crate::sha256_hex;

use crate::local_file::LocalFileBackend;

/// Makes each writer's temporary file name unique within its process. Two
/// concurrent writers of the same block must not share a temp path: both
/// would truncate the same file, and the first rename could publish a
/// half-written block.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The temp sibling one writer fills before renaming it onto the block's
/// path. The process id separates writers in different processes and the
/// counter separates writers within one.
fn temp_block_path(block_path: &Path, hash: &BlockHash) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    block_path.with_file_name(format!("{}.{}.{sequence}.bin.tmp", hash.0, id()))
}

impl LocalFileBackend {
    fn cas_dir(&self) -> PathBuf {
        self.root.join(".cas")
    }

    fn block_path(&self, hash: &BlockHash) -> PathBuf {
        let prefix = hash.0.get(..2).unwrap_or("");
        self.cas_dir().join(prefix).join(format!("{}.bin", hash.0))
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
            return Ok(hash);
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

    /// Collect the block hashes every session log references, so the reclaim
    /// pass can tell a referenced block from an orphan. Scans the JSONL for
    /// the block_ref field instead of parsing events: the pass walks every
    /// session, and a typed parse buys nothing here.
    fn referenced_block_hashes(&self) -> HashSet<String> {
        let mut refs = HashSet::new();
        let Ok(sessions) = fs::read_dir(&self.root) else {
            return refs;
        };
        let needle = "\"block_ref\":\"";
        for entry in sessions.flatten() {
            let log = entry.path().join(LOG_FILE);
            let Ok(content) = fs::read_to_string(&log) else {
                continue;
            };
            let mut at = 0;
            while let Some(i) = content[at..].find(needle) {
                let start = at + i + needle.len();
                let rest = &content[start..];
                let end = rest.find('"').unwrap_or(0);
                if end > 0 {
                    refs.insert(rest[..end].to_string());
                }
                at = start + end + 1;
            }
        }
        refs
    }

    /// Delete the blocks no session log still names: the event carrying that
    /// block_ref was compacted out. Returns how many were removed. A block
    /// any session references stays.
    pub fn reclaim_orphan_blocks(&self) -> Result<usize, ContextError> {
        let referenced = self.referenced_block_hashes();
        let mut removed = 0;
        let Ok(shards) = fs::read_dir(self.cas_dir()) else {
            return Ok(0);
        };
        for shard in shards.flatten() {
            let Ok(blocks) = fs::read_dir(shard.path()) else {
                continue;
            };
            for block in blocks.flatten() {
                let fname = block.file_name();
                let name = match fname.to_str() {
                    Some(n) => n,
                    None => continue,
                };
                // <hash>.bin — strip the suffix for the referenced-set lookup.
                let Some(hash) = name.strip_suffix(".bin") else {
                    // A stale .bin.tmp left by a crash during block_put_sync
                    // (the temp sibling never reached the rename). No
                    // reference can exist for a temp file, so reap it
                    // unconditionally instead of leaking CAS storage.
                    if name.ends_with(".bin.tmp") && fs::remove_file(block.path()).is_ok() {
                        removed += 1;
                    }
                    continue;
                };
                if !referenced.contains(hash) && fs::remove_file(block.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests;
