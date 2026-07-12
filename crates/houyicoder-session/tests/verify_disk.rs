//! Integration tests for SessionStore::verify_disk_chain -- the #15 fix path
//! that hashes raw on-disk line bytes (not a re-serialization) so a
//! cross-binary log with a serde-default schema drift still verifies. The
//! inline unit tests use InMemoryBackend (read_log_range returns empty), so
//! they cannot exercise the disk path; this file uses LocalFileBackend.

#![allow(clippy::unwrap_in_result)]

use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::{SessionStore, SourceChain};
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_root() -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("verify-disk-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&p).expect("mkdir root");
    p
}

fn event(session: SessionId, kind: TurnEventKind) -> TurnEvent {
    TurnEvent {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        kind,
    }
}

/// A chain appended through the write path (hash_event / to_vec) verifies:
/// the raw line bytes the writer produced are the bytes verify_disk_chain
/// hashes, so the chain is byte-stable.
#[tokio::test]
async fn test_disk_chain_verifies_append() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    store
        .append(event(sid, TurnEventKind::UserInput { text: "a".into() }))
        .await
        .expect("append 1");
    store
        .append(event(
            sid,
            TurnEventKind::AssistantMessage {
                text: "b".into(),
                thinking: None,
            },
        ))
        .await
        .expect("append 2");
    assert_eq!(
        store.verify_disk_chain(sid),
        SourceChain::Verified,
        "a chain written by the write path must verify (raw bytes match)",
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A tampered line (the text changed after write) breaks the chain: the
/// recorded prev_hash no longer matches the hash of the tampered line's
/// bytes. This is the tamper-detection guarantee the chain provides.
#[tokio::test]
async fn test_disk_chain_detects_tamper() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    store
        .append(event(
            sid,
            TurnEventKind::UserInput {
                text: "orig".into(),
            },
        ))
        .await
        .expect("append 1");
    store
        .append(event(
            sid,
            TurnEventKind::AssistantMessage {
                text: "reply".into(),
                thinking: None,
            },
        ))
        .await
        .expect("append 2");
    // Verified before tamper.
    assert_eq!(store.verify_disk_chain(sid), SourceChain::Verified);
    // Tamper the FIRST line's text on disk. The second event's recorded
    // prev_hash (hash of the original first line) no longer matches the hash
    // of the tampered first line, so the chain breaks at index 1.
    let log_path = root.join(sid.to_string()).join("log.jsonl");
    let body = std::fs::read_to_string(&log_path).expect("read log");
    let tampered = body.replacen("orig", "TAMPERED", 1);
    std::fs::write(&log_path, tampered).expect("write tampered log");
    match store.verify_disk_chain(sid) {
        SourceChain::Unverified { at_index, .. } => {
            assert_eq!(
                at_index, 1,
                "the break is at the second event (its prev no longer matches the tampered first line)"
            );
        }
        other => panic!("a tampered line must yield Unverified, got {other:?}"),
    }
    std::fs::remove_dir_all(&root).ok();
}

/// An empty session (no log) verifies as Verified (no events to chain).
/// Guards against a regression that treats absence as corruption.
#[tokio::test]
async fn test_disk_chain_empty_verified() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    assert_eq!(
        store.verify_disk_chain(sid),
        SourceChain::Verified,
        "an empty session has no chain to break",
    );
    std::fs::remove_dir_all(&root).ok();
}
