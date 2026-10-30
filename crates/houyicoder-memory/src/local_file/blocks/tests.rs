//! Tests for this module.

use crate::local_file::LocalFileBackend;

use super::temp_block_path;
use houyicoder_context::{BlockHash, ContextBackend, ContextError, SessionId};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::spawn;
use std::time::Duration;

fn temp_root() -> PathBuf {
    let dir = env::temp_dir().join(format!("houyicoder_block_store_test_{}", SessionId::new()));
    fs::create_dir_all(&dir).expect("create temp root");
    dir
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
    let leftovers: Vec<_> = fs::read_dir(block_path.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".bin.tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no .tmp lingers after block_put");
    assert_eq!(
        pollster::block_on(b.block_get(&hash)).unwrap(),
        blob,
        "block round-trips"
    );
    fs::remove_dir_all(&root).ok();
}

/// Reading verifies the bytes against the address. A block edited on disk
/// after the write is corrupt, not a successful read: the stored bytes no
/// longer hash to the name that found them.
#[test]
fn test_block_reads_verify_content() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let hash = pollster::block_on(b.block_put(b"original".to_vec())).unwrap();
    let path = root
        .join(".cas")
        .join(&hash.0[..2])
        .join(format!("{}.bin", hash.0));
    fs::write(&path, b"tampered").unwrap();
    let got = pollster::block_on(b.block_get(&hash));
    assert!(
        matches!(got, Err(ContextError::Corrupt(_))),
        "mismatched bytes must not read as a valid block: {got:?}"
    );
    fs::remove_dir_all(&root).ok();
}

/// A damaged block heals on the next put of the same content. Reads verify
/// the address, so a block edited on disk stays unreadable until something
/// writes it again; the dedup path must not take the damaged file for a
/// stored copy and report success.
#[test]
fn test_put_heals_corrupt_block() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let blob = b"heal me".to_vec();
    let hash = pollster::block_on(b.block_put(blob.clone())).unwrap();
    let path = root
        .join(".cas")
        .join(&hash.0[..2])
        .join(format!("{}.bin", hash.0));
    fs::write(&path, b"damaged").unwrap();
    assert!(
        matches!(
            pollster::block_on(b.block_get(&hash)),
            Err(ContextError::Corrupt(_))
        ),
        "the damaged bytes must not read as the block"
    );
    let again = pollster::block_on(b.block_put(blob.clone())).unwrap();
    assert_eq!(again, hash, "the repaired block keeps its address");
    assert_eq!(
        pollster::block_on(b.block_get(&hash)).unwrap(),
        blob,
        "the block reads again after a write of the same content"
    );
    fs::remove_dir_all(&root).ok();
}

/// A block reference is used as a path component, so anything that is not
/// a sha-256 digest is refused before it can name a file.
#[test]
fn test_bad_hash_rejected() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    for bad in [
        "../../etc/passwd".to_string(),
        "0".repeat(63),
        "A".repeat(64),
    ] {
        let got = pollster::block_on(b.block_get(&BlockHash(bad.clone())));
        assert!(
            matches!(got, Err(ContextError::Corrupt(_))),
            "hash {bad:?} must be refused: {got:?}"
        );
    }
    fs::remove_dir_all(&root).ok();
}

/// Two writers of one block must not share a temporary file. A shared temp
/// lets one writer publish the file while another is still filling it, so a
/// reader of the published path sees a truncated block. Two writers in one
/// process ask for the name twice and get two paths, with no scheduling
/// involved.
#[test]
fn test_block_temp_names_differ() {
    let hash = BlockHash("a".repeat(64));
    let block = PathBuf::from("/sessions/.cas/aa").join(format!("{}.bin", hash.0));
    let first = temp_block_path(&block, &hash);
    let second = temp_block_path(&block, &hash);
    assert_ne!(first, second, "writers must not fill one temp file");
    assert!(
        first.to_string_lossy().ends_with(".bin.tmp"),
        "the temp name is what the reclaim pass recognizes"
    );
}

/// Eight writers fill the same block at once while a reader polls it. Every
/// read either misses or returns the whole block, because each writer fills
/// its own temp file and the rename publishes a finished one. The final read
/// covers the settled state.
#[test]
fn test_concurrent_put_same_block() {
    let root = temp_root();
    let blob = vec![0x5a; 64 * 1024];
    let (tx, rx) = mpsc::channel();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            let blob = blob.clone();
            let tx = tx.clone();
            spawn(move || {
                let b = LocalFileBackend::new(root);
                let hash = pollster::block_on(b.block_put(blob)).unwrap();
                let _sent = tx.send(hash.clone());
                hash
            })
        })
        .collect();
    drop(tx);
    let hash = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a writer reports its address");
    while handles.iter().any(|h| !h.is_finished()) {
        match pollster::block_on(LocalFileBackend::new(root.clone()).block_get(&hash)) {
            Ok(got) => assert_eq!(got, blob, "a published block is never half written"),
            Err(ContextError::NotFound) => {}
            Err(e) => panic!("a read during a concurrent write failed: {e:?}"),
        }
    }
    let hashes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(
        hashes.windows(2).all(|w| w[0] == w[1]),
        "every writer must report the one content address"
    );
    let back = pollster::block_on(LocalFileBackend::new(root.clone()).block_get(&hashes[0]));
    assert_eq!(back.unwrap(), blob, "the published block is whole");
    fs::remove_dir_all(&root).ok();
}

/// A windowed read answers the bytes at the requested offset without loading
/// the rest of the block, reports the stored length so a reader learns the end
/// in one call, and clamps a window that would run past it.
#[test]
fn test_block_range_answers_window() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let blob = b"0123456789".to_vec();
    let hash = pollster::block_on(b.block_put(blob.clone())).unwrap();
    let window = pollster::block_on(b.block_get_range(&hash, 3, 4)).unwrap();
    assert_eq!(window.bytes, b"3456", "the window starts at the offset");
    assert_eq!(window.total, 10, "the length is reported");
    let tail = pollster::block_on(b.block_get_range(&hash, 8, 8)).unwrap();
    assert_eq!(tail.bytes, b"89", "a window stops at the end of the block");
    let past = pollster::block_on(b.block_get_range(&hash, 40, 8)).unwrap();
    assert!(
        past.bytes.is_empty(),
        "an offset past the end reads nothing"
    );
    assert_eq!(past.total, 10, "the length is still reported");
}

/// A window of a damaged block is refused like a whole read: the bytes are
/// checked against the address before any of them is returned, so a block
/// edited on disk cannot be paged past the check.
#[test]
fn test_block_range_verifies_content() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let hash = pollster::block_on(b.block_put(b"original".to_vec())).unwrap();
    let path = root
        .join(".cas")
        .join(&hash.0[..2])
        .join(format!("{}.bin", hash.0));
    fs::write(&path, b"tampered").unwrap();
    let got = pollster::block_on(b.block_get_range(&hash, 0, 4));
    assert!(
        matches!(got, Err(ContextError::Corrupt(_))),
        "a window of damaged bytes must not read: {got:?}"
    );
    let missing = pollster::block_on(b.block_get_range(&BlockHash("0".repeat(64)), 0, 4));
    assert!(
        matches!(missing, Err(ContextError::NotFound)),
        "a block that was never written is missing, not empty: {missing:?}"
    );
    let bad = pollster::block_on(b.block_get_range(&BlockHash("../x".into()), 0, 4));
    assert!(
        matches!(bad, Err(ContextError::Corrupt(_))),
        "a name that is not a digest names no file: {bad:?}"
    );
    fs::remove_dir_all(&root).ok();
}
