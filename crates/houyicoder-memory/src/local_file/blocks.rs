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
            && let Ok(dir_file) = fs::File::open(dir)
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
mod tests {
    use crate::local_file::LocalFileBackend;
    use houyicoder_context::{
        BlockHash, ContextBackend, ContextError, EventId, SessionEvent, SessionId, SessionLogEntry,
    };
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    fn temp_root() -> PathBuf {
        let dir = env::temp_dir().join(format!("houyicoder_block_store_test_{}", SessionId::new()));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn evt(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
        SessionLogEntry {
            id,
            session,
            ts: 0,
            prev_hash: None,
            event: kind,
        }
    }

    /// The block store round-trips, dedups same content, and reports NotFound
    /// for a hash it never held.
    #[test]
    fn test_block_store_round_trips() {
        let root = temp_root();
        let b = LocalFileBackend::new(root.clone());
        let blob = b"hello local cas".to_vec();
        let h1 = pollster::block_on(b.block_put(blob.clone())).unwrap();
        let h2 = pollster::block_on(b.block_put(blob.clone())).unwrap();
        assert_eq!(h1, h2, "same content -> same hash");
        let back = pollster::block_on(b.block_get(&h1)).unwrap();
        assert_eq!(back, blob, "round-trip recovers the bytes");
        let h3 = pollster::block_on(b.block_put(b"other".to_vec())).unwrap();
        assert_ne!(h3, h1, "different content -> different hash");
        let miss = pollster::block_on(b.block_get(&BlockHash("0".repeat(64))));
        assert!(matches!(miss, Err(ContextError::NotFound)));
        fs::remove_dir_all(&root).ok();
    }

    /// A block written by one backend instance is retrievable from a fresh
    /// instance over the same root, and dedup still recognizes it. The store
    /// lives outside the per-session log, so nothing in memory carries it
    /// across a restart. That survival is what a resumed run depends on: a
    /// block reference written before the restart has to resolve after it, or
    /// every resumed event holding one dangles.
    #[test]
    fn test_cas_survives_restart() {
        let root = temp_root();
        let blob = b"restart-durable cas blob".to_vec();
        let hash = {
            let b = LocalFileBackend::new(root.clone());
            pollster::block_on(b.block_put(blob.clone())).unwrap()
        };
        let b2 = LocalFileBackend::new(root.clone());
        let back = pollster::block_on(b2.block_get(&hash)).unwrap();
        assert_eq!(back, blob, "CAS block survives a backend restart");
        let again = pollster::block_on(b2.block_put(blob.clone())).unwrap();
        assert_eq!(again, hash, "dedup stable across restart");
        fs::remove_dir_all(&root).ok();
    }

    /// Crash atomicity: a block write goes to a temp sibling, fsyncs, then
    /// renames, so a reader never observes a partial block. Once the call
    /// returns no temp file lingers and the bytes read back. A real crash
    /// mid-write does leave the temp sibling behind, which the reclaim pass
    /// reaps.
    #[test]
    fn test_cas_crash_atomicity() {
        let root = temp_root();
        let b = LocalFileBackend::new(root.clone());
        let blob = b"atomicity-checked blob".to_vec();
        let hash = pollster::block_on(b.block_put(blob.clone())).unwrap();
        let block_path = root
            .join(".cas")
            .join(&hash.0[..2])
            .join(format!("{}.bin", hash.0));
        assert!(block_path.exists(), "block file present");
        assert!(
            !block_path.with_extension("bin.tmp").exists(),
            "no .tmp lingers after block_put"
        );
        assert_eq!(
            pollster::block_on(b.block_get(&hash)).unwrap(),
            blob,
            "block round-trips"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// A block some session log references survives the reclaim; a block none
    /// references does not.
    #[test]
    fn test_reclaim_blocks_removes_unreferenced() {
        let root = temp_root();
        let b = LocalFileBackend::new(root.clone());
        let session = SessionId::new();
        // A block a session log references, so the scan finds it and keeps it.
        let referenced_blob = b"keep me".to_vec();
        let referenced_hash = pollster::block_on(b.block_put(referenced_blob.clone())).unwrap();
        pollster::block_on(b.append(evt(
            session,
            EventId::new(),
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"block_ref": referenced_hash.0, "preview": "..."}),
                duration_ms: 0,
            },
        )))
        .unwrap();
        // An orphan block: put directly, no log references it.
        let orphan_blob = b"reclaim me".to_vec();
        let orphan_hash = pollster::block_on(b.block_put(orphan_blob.clone())).unwrap();
        let orphan_path = root
            .join(".cas")
            .join(&orphan_hash.0[..2])
            .join(format!("{}.bin", orphan_hash.0));
        assert!(orphan_path.exists(), "orphan present before reclaim");
        let removed = b.reclaim_orphan_blocks().unwrap();
        assert_eq!(removed, 1, "the orphan was reclaimed");
        assert!(!orphan_path.exists(), "orphan gone after reclaim");
        let kept_path = root
            .join(".cas")
            .join(&referenced_hash.0[..2])
            .join(format!("{}.bin", referenced_hash.0));
        assert!(kept_path.exists(), "referenced block stays");
        assert_eq!(
            pollster::block_on(b.block_get(&referenced_hash)).unwrap(),
            referenced_blob,
            "referenced block still retrievable"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// A crash mid-write leaves the temp sibling on disk. No reference can
    /// name a file that never reached the rename, so the reclaim reaps it
    /// rather than leaking storage.
    #[test]
    fn test_reclaim_reaps_stale_tmp() {
        let root = temp_root();
        let b = LocalFileBackend::new(root.clone());
        let session = SessionId::new();
        // A referenced block, which the same pass must leave alone.
        let blob = b"keep me".to_vec();
        let hash = pollster::block_on(b.block_put(blob.clone())).unwrap();
        pollster::block_on(b.append(evt(
            session,
            EventId::new(),
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"block_ref": hash.0, "preview": "..."}),
                duration_ms: 0,
            },
        )))
        .unwrap();
        // Simulate a crash mid-write: drop a .bin.tmp temp sibling directly.
        let tmp_path = root
            .join(".cas")
            .join(&hash.0[..2])
            .join(format!("{}.bin.tmp", hash.0));
        fs::write(&tmp_path, b"stale temp").unwrap();
        assert!(tmp_path.exists(), "stale .bin.tmp present before reclaim");
        let removed = b.reclaim_orphan_blocks().unwrap();
        assert_eq!(removed, 1, "the stale .bin.tmp was reaped");
        assert!(!tmp_path.exists(), "stale .bin.tmp gone after reclaim");
        assert_eq!(
            pollster::block_on(b.block_get(&hash)).unwrap(),
            blob,
            "referenced block still retrievable"
        );
        fs::remove_dir_all(&root).ok();
    }
}
