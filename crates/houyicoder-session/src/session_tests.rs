use std::env::temp_dir;
use std::fs::{OpenOptions, create_dir_all, metadata, read_to_string, remove_dir_all, write};
use std::io::Write;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, SessionEvent};
use houyicoder_memory::{InMemoryBackend, LocalFileBackend};

fn child_return(input: u64, output: u64, cache_read: u64) -> SessionEvent {
    SessionEvent::SubagentReturn {
        child_session_id: "child".into(),
        status: "completed".into(),
        summary: String::new(),
        result_ref: "child".into(),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
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

async fn appended_event(
    store: &SessionStore,
    session: SessionId,
    kind: SessionEvent,
) -> SessionLogEntry {
    // Returns the event as SessionStore stored it (with prev_hash set), by
    // appending then replaying the last. This is the bytes that next link hashes.
    let id = EventId::new();
    let e = evt(session, id, kind);
    store.append(e.clone()).await.unwrap();
    let replay = store.replay(session).await.unwrap();
    replay.last().unwrap().clone()
}

#[tokio::test]
async fn test_append_sets_hash_chain() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let s = SessionId::new();
    let e1_stored = appended_event(&store, s, SessionEvent::UserInput { text: "a".into() }).await;
    let e2_stored = appended_event(
        &store,
        s,
        SessionEvent::AssistantMessage {
            text: "b".into(),
            thinking: None,
        },
    )
    .await;
    let e3_stored = appended_event(&store, s, SessionEvent::Reasoning { text: "c".into() }).await;
    // First event: no previous.
    assert!(e1_stored.prev_hash.is_none());
    // e2.prev_hash == H(e1 stored).
    assert_eq!(
        e2_stored.prev_hash,
        Some(SessionStore::hash_event(&e1_stored).unwrap())
    );
    // e3.prev_hash == H(e2 stored, including e2's own prev_hash) — recursive.
    assert_eq!(
        e3_stored.prev_hash,
        Some(SessionStore::hash_event(&e2_stored).unwrap())
    );
}

#[tokio::test]
async fn test_wakeup_retained() {
    let signal = Arc::new(tokio::sync::Notify::new());
    let store =
        SessionStore::new(Box::new(InMemoryBackend::new())).with_append_notify(signal.clone());
    let session = SessionId::new();
    store
        .append(evt(
            session,
            EventId::new(),
            SessionEvent::UserInput {
                text: "queued".into(),
            },
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(20), signal.notified())
        .await
        .expect("an append remains observable when the receiver polls after it");
}

#[tokio::test]
async fn test_trajectory_keeps_order() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let s = SessionId::new();
    let e1 = appended_event(&store, s, SessionEvent::UserInput { text: "a".into() }).await;
    let e2 = appended_event(
        &store,
        s,
        SessionEvent::AssistantMessage {
            text: "b".into(),
            thinking: None,
        },
    )
    .await;
    let e3 = appended_event(&store, s, SessionEvent::Reasoning { text: "c".into() }).await;
    let traj = store.trajectory_snapshot(s);
    assert_eq!(traj.len(), 3, "mirror holds every appended event");
    assert!(traj[0].prev_hash.is_none(), "first link has no predecessor");
    assert_eq!(
        traj[1].prev_hash,
        Some(SessionStore::hash_event(&e1).unwrap()),
        "second link hashes the first finalized event"
    );
    assert_eq!(
        traj[2].prev_hash,
        Some(SessionStore::hash_event(&e2).unwrap()),
        "third link hashes the second finalized event"
    );
    assert_eq!(
        traj[2].prev_hash, e3.prev_hash,
        "mirror row matches the finalized event append returned"
    );
    // The mirror is per-session: a different session reads empty until it
    // appends.
    let other = SessionId::new();
    assert!(store.trajectory_snapshot(other).is_empty());
    // reset_trajectory frees the mirror without touching the backend log.
    store.reset_trajectory(s);
    assert!(store.trajectory_snapshot(s).is_empty());
    assert_eq!(
        store.replay(s).await.unwrap().len(),
        3,
        "backend log survives a mirror reset"
    );
}

#[tokio::test]
async fn test_subagent_usage_tracks_view() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let session = SessionId::new();
    appended_event(
        &store,
        session,
        SessionEvent::UserInput { text: "a".into() },
    )
    .await;
    appended_event(&store, session, child_return(100, 20, 80)).await;
    appended_event(&store, session, child_return(50, 5, 0)).await;
    let usage = store.subagent_usage(session);
    assert_eq!(usage.calls, 2);
    assert_eq!(usage.input_tokens, 150);
    assert_eq!(usage.output_tokens, 25);
    assert_eq!(usage.cache_read_input_tokens, 80);
    store.reset_trajectory(session);
    assert_eq!(
        store.subagent_usage(session),
        SubagentUsage::default(),
        "clear resets the projection with the view"
    );
}

#[tokio::test]
async fn test_subagent_usage_restores() {
    let root = temp_dir().join(format!("usage-restore-{}-{}", id(), EventId::new()));
    create_dir_all(&root).expect("mkdir root");
    let session = SessionId::new();
    {
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        appended_event(&store, session, child_return(200, 30, 120)).await;
    }
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    assert_eq!(
        store.subagent_usage(session).calls,
        0,
        "cold store is empty"
    );
    assert_eq!(store.restore_trajectory(session).await.unwrap(), 1);
    let usage = store.subagent_usage(session);
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.input_tokens, 200);
    assert_eq!(usage.output_tokens, 30);
    assert_eq!(usage.cache_read_input_tokens, 120);
}

#[tokio::test]
async fn test_usage_through_trait() {
    // The status handler holds Arc<dyn SessionLog>, so it reaches the trait
    // method; the inherent method is not the path it takes.
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let session = SessionId::new();
    appended_event(&store, session, child_return(70, 8, 30)).await;
    let log: Arc<dyn SessionLog> = Arc::new(store);
    let usage = log.subagent_usage(session);
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.input_tokens, 70);
    assert_eq!(usage.output_tokens, 8);
}

#[tokio::test]
async fn test_last_id_tracks_mirror() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let s = SessionId::new();
    assert_eq!(store.last_trajectory_id(s), None, "no mirror, no id");
    appended_event(&store, s, SessionEvent::UserInput { text: "a".into() }).await;
    let e2 = appended_event(&store, s, SessionEvent::Reasoning { text: "c".into() }).await;
    assert_eq!(
        store.last_trajectory_id(s),
        Some(e2.id),
        "the tail follows the append"
    );
    let other = SessionId::new();
    assert_eq!(
        store.last_trajectory_id(other),
        None,
        "the mirror is per-session"
    );
    // Through the trait object: the override answers what the inherent read
    // answers.
    let log: Arc<dyn SessionLog> = Arc::new(store);
    assert_eq!(log.last_trajectory_id(s), Some(e2.id));
    assert_eq!(log.last_trajectory_id(other), None);
}

#[tokio::test]
async fn test_view_returns_replay() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let s = SessionId::new();
    store
        .append(evt(
            s,
            EventId::new(),
            SessionEvent::UserInput { text: "hi".into() },
        ))
        .await
        .unwrap();
    let snap = store.current_view(s).await.unwrap();
    assert_eq!(snap.session, s);
    assert_eq!(snap.events.len(), 1);
    assert!(snap.last_checkpoint.is_none());
    assert!(snap.rewind_points.is_empty());
    assert!(snap.manifest.is_none(), "no manifest without a checkpoint");
}

#[tokio::test]
async fn test_rewind_persisted_counter() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let s = SessionId::new();
    store.mark_persisted(s, 5);
    assert_eq!(store.rewind_persisted(s, 2), Some(3));
    assert_eq!(store.rewind_persisted(s, 100), Some(0)); // saturates
    assert_eq!(store.rewind_persisted(SessionId::new(), 1), None); // unknown
}

/// Build a source event list with a valid prev_hash chain (each event's
/// prev_hash = hash_line_bytes of the previous event's compact line
/// bytes, including the previous event's own prev_hash). Mirrors what an
/// exporting binary writes. Deltas are included in the chain as the
/// exporter recorded them.
fn chained_source(session: SessionId, kinds: Vec<SessionEvent>) -> Vec<SessionLogEntry> {
    let mut out = Vec::new();
    let mut prev: Option<PrevHash> = None;
    for kind in kinds {
        let ev = SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: prev,
            event: kind,
        };
        let bytes = serde_json::to_vec(&ev).unwrap();
        prev = Some(SessionStore::hash_line_bytes(&bytes));
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn test_seed_drops_text_delta() {
    let src_session = SessionId::new();
    let source = chained_source(
        src_session,
        vec![
            SessionEvent::UserInput { text: "hi".into() },
            SessionEvent::AssistantTextDelta { text: "par".into() },
            SessionEvent::AssistantMessage {
                text: "hi".into(),
                thinking: None,
            },
        ],
    );
    let dest = SessionStore::new(Box::new(InMemoryBackend::new()));
    let dest_session = SessionId::new();
    let report = dest
        .seed_trajectory(dest_session, source.clone())
        .await
        .expect("seed");
    assert_eq!(
        report.durable_count, 2,
        "durable UserInput + AssistantMessage"
    );
    assert_eq!(report.deltas_dropped, 1, "the streaming delta is dropped");
    assert_eq!(report.source_chain, SourceChain::Verified);
    let replayed = dest.replay(dest_session).await.expect("replay");
    assert_eq!(replayed.len(), 2, "durable log carries no delta");
    assert!(
        !replayed
            .iter()
            .any(|e| matches!(e.event, SessionEvent::AssistantTextDelta { .. })),
        "delta must not be in the durable log"
    );
    // head_hash is the rebuilt durable chain's last hash (hash of the
    // last replayed event's line bytes, with the rebuilt prev_hash).
    let last_bytes = serde_json::to_vec(replayed.last().unwrap()).unwrap();
    assert_eq!(
        report.head_hash,
        Some(SessionStore::hash_line_bytes(&last_bytes)),
        "head_hash matches the rebuilt durable chain tail"
    );
}

#[tokio::test]
async fn test_seed_unverified_source_rebuilds() {
    let src_session = SessionId::new();
    let mut source = chained_source(
        src_session,
        vec![
            SessionEvent::UserInput { text: "hi".into() },
            SessionEvent::AssistantMessage {
                text: "hi".into(),
                thinking: None,
            },
        ],
    );
    // Tamper the second event's prev_hash so the source chain breaks.
    source[1].prev_hash = Some(PrevHash([0u8; 32]));
    let dest = SessionStore::new(Box::new(InMemoryBackend::new()));
    let dest_session = SessionId::new();
    let report = dest
        .seed_trajectory(dest_session, source.clone())
        .await
        .expect("seed never hard-fails on an unverified source");
    assert!(
        matches!(
            report.source_chain,
            SourceChain::Unverified { at_index: 1, .. }
        ),
        "source chain is unverified at the tampered index"
    );
    // The rebuilt durable chain is still internally consistent: two
    // durable events replay with a valid chain.
    let replayed = dest.replay(dest_session).await.expect("replay");
    assert_eq!(
        replayed.len(),
        2,
        "durable chain rebuilt despite unverified source"
    );
    assert_eq!(
        verify_source_chain_inline(&replayed),
        SourceChain::Verified,
        "rebuilt durable chain is internally verified"
    );
}

fn verify_source_chain_inline(events: &[SessionLogEntry]) -> SourceChain {
    // Same logic as SessionStore::verify_source_chain, exercised here on
    // the rebuilt durable chain to assert internal consistency.
    let mut prev: Option<PrevHash> = None;
    for (i, ev) in events.iter().enumerate() {
        let Ok(bytes) = serde_json::to_vec(ev) else {
            return SourceChain::Unverified {
                at_index: i,
                reason: "serialize failed".into(),
            };
        };
        let h = SessionStore::hash_line_bytes(&bytes);
        if ev.prev_hash != prev {
            return SourceChain::Unverified {
                at_index: i,
                reason: "prev_hash does not chain".into(),
            };
        }
        prev = Some(h);
    }
    SourceChain::Verified
}

#[cfg(test)]
mod disk_verify {
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
}

/// An empty source (an export with no trajectory events) seeds zero durable
/// events + a fresh session: no crash, no phantom events. The rebuilt chain
/// is trivially Verified (no events to chain). Mirrors resuming an empty /
/// just-started export.
#[tokio::test]
async fn test_seed_empty_source_fresh() {
    let src_session = SessionId::new();
    let source = chained_source(src_session, vec![]);
    let dest = SessionStore::new(Box::new(InMemoryBackend::new()));
    let dest_session = SessionId::new();
    let report = dest
        .seed_trajectory(dest_session, source)
        .await
        .expect("seed of an empty source must not error");
    assert_eq!(report.durable_count, 0, "no events seeded");
    assert_eq!(report.deltas_dropped, 0);
    assert_eq!(
        report.source_chain,
        SourceChain::Verified,
        "an empty chain is trivially verified"
    );
    let replayed = dest.replay(dest_session).await.expect("replay");
    assert!(replayed.is_empty(), "no durable events in the dest log");
}

/// A fork chain accumulates history: seed B from A's source, then seed C
/// from B's replay (A's events + B's new), then C carries A's history
/// forward. Pins that successive seeds do not lose the originating events
/// (the resume->resume->resume chain, at the seed level -- no multi-binary
/// PTY needed).
#[tokio::test]
async fn test_fork_chain_accumulates_history() {
    let src_session = SessionId::new();
    let source_a = chained_source(
        src_session,
        vec![SessionEvent::UserInput {
            text: "A's prompt".into(),
        }],
    );
    // B = fork from A.
    let store_b = SessionStore::new(Box::new(InMemoryBackend::new()));
    let sid_b = SessionId::new();
    store_b
        .seed_trajectory(sid_b, source_a.clone())
        .await
        .expect("seed B from A");
    // C = fork from B (B's replay = A's events).
    let store_c = SessionStore::new(Box::new(InMemoryBackend::new()));
    let sid_c = SessionId::new();
    let b_replay = store_b.replay(sid_b).await.expect("replay B");
    store_c
        .seed_trajectory(sid_c, b_replay)
        .await
        .expect("seed C from B");
    let c_replay = store_c.replay(sid_c).await.expect("replay C");
    assert_eq!(c_replay.len(), 1, "C carries A's single event forward");
    assert!(
        c_replay.iter().any(|e| matches!(
            e.event,
            SessionEvent::UserInput { ref text } if text == "A's prompt"
        )),
        "C's history must contain A's originating prompt (chain accumulation)"
    );
}

/// An export->resume->export->resume roundtrip preserves history: seed B
/// from A, append a new event to B, seed C from B's replay (A's + B's new),
/// + C carries both. Pins no loss across two seed cycles.
#[tokio::test]
async fn test_seed_roundtrip_preserves_history() {
    let src_session = SessionId::new();
    let source_a = chained_source(
        src_session,
        vec![SessionEvent::UserInput {
            text: "first".into(),
        }],
    );
    let store_b = SessionStore::new(Box::new(InMemoryBackend::new()));
    let sid_b = SessionId::new();
    store_b
        .seed_trajectory(sid_b, source_a)
        .await
        .expect("seed B from A");
    // Append a new durable event to B (a continued turn after resume).
    store_b
        .append(SessionLogEntry {
            id: EventId::new(),
            session: sid_b,
            ts: 1,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "second".into(),
            },
        })
        .await
        .expect("append to B");
    // C = resume from B's export (B's replay = first + second).
    let store_c = SessionStore::new(Box::new(InMemoryBackend::new()));
    let sid_c = SessionId::new();
    let b_replay = store_b.replay(sid_b).await.expect("replay B");
    store_c
        .seed_trajectory(sid_c, b_replay)
        .await
        .expect("seed C from B");
    let c_replay = store_c.replay(sid_c).await.expect("replay C");
    assert_eq!(
        c_replay.len(),
        2,
        "C carries both events (no loss across cycles)"
    );
    let texts: Vec<&str> = c_replay
        .iter()
        .map(|e| match &e.event {
            SessionEvent::UserInput { text } => text.as_str(),
            _ => "?",
        })
        .collect();
    assert!(texts.contains(&"first"), "first event preserved");
    assert!(texts.contains(&"second"), "second event preserved");
}

/// read_child_result reads the child's full log from disk; a missing child
/// degrades to empty.
#[tokio::test]
async fn test_read_child_result() {
    let root = temp_dir().join(format!("child-result-unit-{}", id()));
    create_dir_all(&root).expect("mkdir");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let child = SessionId::new();
    store
        .append(evt(
            child,
            EventId::new(),
            SessionEvent::UserInput { text: "go".into() },
        ))
        .await
        .expect("append");
    let result = store.read_child_result(child);
    assert!(!result.is_empty(), "child result should have events");
    assert!(store.read_child_result(SessionId::new()).is_empty());
    remove_dir_all(&root).ok();
}

/// Cold prev_hash (cache miss) hashes the raw last disk line via reverse-read,
/// not a re-serialization of the replayed last event. Uses a LocalFileBackend
/// so the reverse-read path is real (InMemoryBackend returns empty, hitting
/// the fallback).
#[tokio::test]
async fn test_prev_hash_reads_raw() {
    let root = temp_dir().join(format!(
        "cold-prev-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    drop(appended_event(&store, sid, SessionEvent::UserInput { text: "a".into() }).await);
    let e2 = appended_event(
        &store,
        sid,
        SessionEvent::AssistantMessage {
            text: "b".into(),
            thinking: None,
        },
    )
    .await;
    store.last_hashes.lock().unwrap().clear();
    let cold = store.compute_prev_hash(sid).await.unwrap();
    let rr = store.backend().read_lines_reverse(sid, u64::MAX, 1_048_576);
    let last_line = rr.lines.first().expect("last line").1.clone();
    assert_eq!(
        cold,
        Some(SessionStore::hash_line_bytes(last_line.as_bytes())),
        "cold path must hash the raw last disk line bytes",
    );
    assert_eq!(
        cold,
        Some(SessionStore::hash_event(&e2).unwrap()),
        "within one binary, raw line bytes match re-serialization",
    );
    remove_dir_all(&root).ok();
}

/// The cold prev_hash stays byte-stable under a serde schema drift: a line
/// carrying an unknown field (a prior binary wrote it) parses, but
/// re-serialization omits the field and drifts. The cold path must hash the
/// raw line bytes, not the re-serialized reparsed event.
#[tokio::test]
async fn test_prev_hash_survives_drift() {
    let root = temp_dir().join(format!(
        "cold-drift-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    drop(appended_event(&store, sid, SessionEvent::UserInput { text: "a".into() }).await);
    let rr = store.backend().read_lines_reverse(sid, u64::MAX, 1_048_576);
    let orig_line = rr.lines.first().expect("last line").1.clone();
    // Insert an unknown field a prior binary's schema carried; the current
    // schema ignores it on parse, re-serialization omits it.
    let brace = orig_line.rfind('}').unwrap();
    let drifted_line = format!("{},\"zz_future_drift\":0}}", &orig_line[..brace]);
    let log_path = root.join(sid.to_string()).join("log.jsonl");
    write(&log_path, format!("{drifted_line}\n")).unwrap();
    store.last_hashes.lock().unwrap().clear();
    let cold = store.compute_prev_hash(sid).await.unwrap();
    assert_eq!(
        cold,
        Some(SessionStore::hash_line_bytes(drifted_line.as_bytes())),
        "cold path must hash the raw line bytes (stable across schema drift)",
    );
    let reparsed = store.replay(sid).await.unwrap();
    let drifted_hash = SessionStore::hash_event(&reparsed[0]).unwrap();
    assert_ne!(
        cold,
        Some(drifted_hash),
        "cold path must not use re-serialization of the reparsed event",
    );
    remove_dir_all(&root).ok();
}

/// A newest entry wide enough to span several reverse walks: the read reaches
/// its head only through the read's byte budget, and the cold path must hash
/// the bytes of the line the log ends with, not of an older line the walk
/// stopped on.
#[tokio::test]
async fn test_wide_last_line_hash() {
    let root = temp_dir().join(format!(
        "cold-wide-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    drop(appended_event(&store, sid, SessionEvent::UserInput { text: "a".into() }).await);
    // Wider than the 64 KB the reverse read walks per chunk.
    let wide = appended_event(
        &store,
        sid,
        SessionEvent::UserInput {
            text: "z".repeat(100_000),
        },
    )
    .await;
    let log_path = root.join(sid.to_string()).join("log.jsonl");
    let raw = read_to_string(&log_path).unwrap();
    let last_line = raw.lines().last().expect("the log holds lines").to_string();
    store.last_hashes.lock().unwrap().clear();
    let cold = store.compute_prev_hash(sid).await.unwrap();
    assert_eq!(
        cold,
        Some(SessionStore::hash_line_bytes(last_line.as_bytes())),
        "cold path must hash the line the log ends with",
    );
    assert_eq!(
        cold,
        Some(SessionStore::hash_event(&wide).unwrap()),
        "the hash still matches the event that was appended",
    );
    assert_ne!(
        cold,
        Some(SessionStore::hash_line_bytes(first_line_bytes(&raw))),
        "cold path must not fall back to the first line of the log",
    );
    remove_dir_all(&root).ok();
}

/// The bytes of the log's first line, for probing which line a hash came from.
fn first_line_bytes(raw: &str) -> &[u8] {
    raw.lines().next().unwrap_or_default().as_bytes()
}

/// A newest entry wider than the cold path's reverse-read budget: the read
/// reports that it did not reach that line, so the cold path replays and
/// hashes the event it parsed. Taking the newest line the batch did hold
/// instead links the next entry to the line before the last, and the chain
/// breaks with every later entry still chaining.
#[tokio::test]
async fn test_wide_over_budget_hash() {
    let root = temp_dir().join(format!(
        "cold-over-budget-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    drop(appended_event(&store, sid, SessionEvent::UserInput { text: "a".into() }).await);
    // Wider than the 1 MiB the cold path reads.
    let wide = appended_event(
        &store,
        sid,
        SessionEvent::UserInput {
            text: "z".repeat(1_500_000),
        },
    )
    .await;
    let log_path = root.join(sid.to_string()).join("log.jsonl");
    let raw = read_to_string(&log_path).unwrap();
    store.last_hashes.lock().unwrap().clear();
    let cold = store.compute_prev_hash(sid).await.unwrap();
    assert_eq!(
        cold,
        Some(SessionStore::hash_event(&wide).unwrap()),
        "the cold path must link to the line the log ends with",
    );
    assert_ne!(
        cold,
        Some(SessionStore::hash_line_bytes(first_line_bytes(&raw))),
        "cold path must not fall back to the line before the last",
    );
    remove_dir_all(&root).ok();
}

/// A log torn mid-write: the trailing bytes no terminator follows hold no
/// line, so the newest whole line the log holds is the one before them and
/// the cold path hashes it. A read that rejected the log for its missing
/// terminator would replay instead, fail on the partial bytes, and leave the
/// session unable to append at all.
#[tokio::test]
async fn test_torn_tail_hash() {
    let root = temp_dir().join(format!(
        "cold-torn-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    drop(
        appended_event(
            &store,
            sid,
            SessionEvent::UserInput {
                text: "first".into(),
            },
        )
        .await,
    );
    drop(
        appended_event(
            &store,
            sid,
            SessionEvent::UserInput {
                text: "second".into(),
            },
        )
        .await,
    );
    let log_path = root.join(sid.to_string()).join("log.jsonl");
    let size = metadata(&log_path).unwrap().len();
    let f = OpenOptions::new().write(true).open(&log_path).unwrap();
    f.set_len(size - 5).unwrap();
    let raw = read_to_string(&log_path).unwrap();
    store.last_hashes.lock().unwrap().clear();
    let cold = store.compute_prev_hash(sid).await.unwrap();
    assert_eq!(
        cold,
        Some(SessionStore::hash_line_bytes(first_line_bytes(&raw))),
        "the last whole line the log holds is its first",
    );
    // The append after the tear still lands: the cold path reads the line the
    // log holds rather than failing on the bytes it does not.
    store.last_hashes.lock().unwrap().clear();
    store
        .append(evt(
            sid,
            EventId::new(),
            SessionEvent::UserInput {
                text: "third".into(),
            },
        ))
        .await
        .unwrap();
    remove_dir_all(&root).ok();
}
