//! The log's verify pass: it re-reads the raw bytes and re-hashes the chain,
//! reporting the first index where it breaks, rather than trusting the index.

use super::*;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("verify-disk-lib-{}-{n}", id()));
    create_dir_all(&p).expect("mkdir root");
    p
}

fn ev(session: SessionId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

/// A chain written by the write path verifies: verify_disk_chain hashes
/// the raw on-disk line bytes, which are the bytes the writer hashed.
/// This is the #15 fix -- re-serializing would drift across schema
/// changes; raw bytes are byte-stable.
#[tokio::test]
async fn test_write_path_chain_verifies() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    store
        .append(ev(sid, SessionEvent::UserInput { text: "a".into() }))
        .await
        .expect("append 1");
    store
        .append(ev(
            sid,
            SessionEvent::AssistantMessage {
                text: "b".into(),
                thinking: None,
            },
        ))
        .await
        .expect("append 2");
    assert_eq!(store.verify_disk_chain(sid), SourceChain::Verified);
    remove_dir_all(&root).ok();
}

/// Tampering a line's text on disk breaks the chain at the next event
/// (its recorded prev_hash no longer matches the tampered line's hash).
#[tokio::test]
async fn test_tamper_breaks_chain() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    store
        .append(ev(
            sid,
            SessionEvent::UserInput {
                text: "orig".into(),
            },
        ))
        .await
        .expect("append 1");
    store
        .append(ev(
            sid,
            SessionEvent::AssistantMessage {
                text: "r".into(),
                thinking: None,
            },
        ))
        .await
        .expect("append 2");
    assert_eq!(store.verify_disk_chain(sid), SourceChain::Verified);
    let log = root.join(sid.to_string()).join("log.jsonl");
    let body = read_to_string(&log).expect("read");
    write(&log, body.replacen("orig", "TAMPERED", 1)).expect("write");
    match store.verify_disk_chain(sid) {
        SourceChain::Unverified { at_index, .. } => assert_eq!(at_index, 1),
        other => panic!("tamper must break the chain: {other:?}"),
    }
    remove_dir_all(&root).ok();
}

/// A line that fails to parse (corrupt JSON) yields Unverified at that
/// index, not a panic -- the verify is best-effort.
#[tokio::test]
async fn test_corrupt_line_unverified() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    store
        .append(ev(sid, SessionEvent::UserInput { text: "ok".into() }))
        .await
        .expect("append");
    // Append a garbage line after the valid one.
    let log = root.join(sid.to_string()).join("log.jsonl");
    let mut f = OpenOptions::new().append(true).open(&log).unwrap();
    f.write_all(b"not-json\n").unwrap();
    match store.verify_disk_chain(sid) {
        SourceChain::Unverified { .. } => {}
        other => panic!("a corrupt line must yield Unverified, got {other:?}"),
    }
    remove_dir_all(&root).ok();
}

/// After seeding a session from an export (the resume-from-export path),
/// a subsequent append through the write path must keep the on-disk chain
/// verified. Proves the chain continues correctly when the conversation
/// resumes (the seeded tail links to the next appended event). Uses
/// LocalFileBackend so verify_disk_chain reads the raw line bytes.
#[tokio::test]
async fn test_append_after_seed_verifies() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let src_sid = SessionId::new();
    let dest_sid = SessionId::new();
    let source = chained_source(
        src_sid,
        vec![
            SessionEvent::UserInput {
                text: "seeded".into(),
            },
            SessionEvent::AssistantMessage {
                text: "reply".into(),
                thinking: None,
            },
        ],
    );
    let report = store.seed_trajectory(dest_sid, source).await.expect("seed");
    assert_eq!(report.durable_count, 2);
    assert_eq!(store.verify_disk_chain(dest_sid), SourceChain::Verified);
    // Append a new event after the seed (the resumed conversation).
    store
        .append(ev(
            dest_sid,
            SessionEvent::UserInput {
                text: "after resume".into(),
            },
        ))
        .await
        .expect("append after seed");
    // The chain still verifies: the appended event's prev_hash links to
    // the seeded tail's raw disk bytes.
    assert_eq!(
        store.verify_disk_chain(dest_sid),
        SourceChain::Verified,
        "chain must stay verified after a post-seed append"
    );
    remove_dir_all(&root).ok();
}
