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
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::{BlockHash, ContextError, session_class::LOG_FILE};

use crate::sha256_hex;

use crate::local_file::LocalFileBackend;
use fs::File;

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
        // Dedup: same content already on disk; do not rewrite.
        if path.exists() {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            Self::ensure_dir(parent)?;
        }
        // Atomic write to a temp sibling + fsync + rename so a crash never
        // leaves a partial block (a reader gets the complete block or nothing).
        let tmp = path.with_extension("bin.tmp");
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&tmp).map_err(|_| ContextError::Io)?;
        f.write_all(&block).map_err(|_| ContextError::Io)?;
        f.sync_all().map_err(|_| ContextError::Io)?;
        drop(f);
        fs::rename(&tmp, &path).map_err(|_| ContextError::Io)?;
        // Fsync the parent directory so the rename is durable across a
        // crash; otherwise the block's data is fsynced but its directory
        // entry is not, leaving a dangling block_ref on restart.
        if let Some(dir) = path.parent()
            && let Ok(dir_file) = File::open(dir)
        {
            drop(dir_file.sync_all());
        }
        Ok(hash)
    }

    pub(super) fn block_get_sync(&self, hash: &BlockHash) -> Result<Vec<u8>, ContextError> {
        let path = self.block_path(hash);
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
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
