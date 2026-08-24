use super::*;
use houyicoder_context::{EventId, TurnEventKind};
use houyicoder_memory::{InMemoryBackend, LocalFileBackend};

fn evt(session: SessionId, id: EventId, kind: TurnEventKind) -> TurnEvent {
    TurnEvent {
        id,
        session,
        ts: 0,
        prev_hash: None,
        kind,
    }
}

async fn appended_event(
    store: &SessionStore,
    session: SessionId,
    kind: TurnEventKind,
) -> TurnEvent {
    // Returns the event as SessionStore stored it (with prev_hash set), by
    // appending then replaying the last. This is the bytes the next link hashes.
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
    let e1_stored = appended_event(&store, s, TurnEventKind::UserInput { text: "a".into() }).await;
    let e2_stored = appended_event(
        &store,
        s,
        TurnEventKind::AssistantMessage {
            text: "b".into(),
            thinking: None,
        },
    )
    .await;
    let e3_stored = appended_event(&store, s, TurnEventKind::Reasoning { text: "c".into() }).await;
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
async fn test_trajectory_keeps_order() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let s = SessionId::new();
    let e1 = appended_event(&store, s, TurnEventKind::UserInput { text: "a".into() }).await;
    let e2 = appended_event(
        &store,
        s,
        TurnEventKind::AssistantMessage {
            text: "b".into(),
            thinking: None,
        },
    )
    .await;
    let e3 = appended_event(&store, s, TurnEventKind::Reasoning { text: "c".into() }).await;
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
async fn test_view_returns_replay() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let s = SessionId::new();
    store
        .append(evt(
            s,
            EventId::new(),
            TurnEventKind::UserInput { text: "hi".into() },
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
fn chained_source(session: SessionId, kinds: Vec<TurnEventKind>) -> Vec<TurnEvent> {
    let mut out = Vec::new();
    let mut prev: Option<PrevHash> = None;
    for kind in kinds {
        let ev = TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: prev,
            kind,
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
            TurnEventKind::UserInput { text: "hi".into() },
            TurnEventKind::AssistantTextDelta { text: "par".into() },
            TurnEventKind::AssistantMessage {
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
            .any(|e| matches!(e.kind, TurnEventKind::AssistantTextDelta { .. })),
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
            TurnEventKind::UserInput { text: "hi".into() },
            TurnEventKind::AssistantMessage {
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

fn verify_source_chain_inline(events: &[TurnEvent]) -> SourceChain {
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
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("verify-disk-lib-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("mkdir root");
        p
    }

    fn ev(session: SessionId, kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind,
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
            .append(ev(sid, TurnEventKind::UserInput { text: "a".into() }))
            .await
            .expect("append 1");
        store
            .append(ev(
                sid,
                TurnEventKind::AssistantMessage {
                    text: "b".into(),
                    thinking: None,
                },
            ))
            .await
            .expect("append 2");
        assert_eq!(store.verify_disk_chain(sid), SourceChain::Verified);
        std::fs::remove_dir_all(&root).ok();
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
                TurnEventKind::UserInput {
                    text: "orig".into(),
                },
            ))
            .await
            .expect("append 1");
        store
            .append(ev(
                sid,
                TurnEventKind::AssistantMessage {
                    text: "r".into(),
                    thinking: None,
                },
            ))
            .await
            .expect("append 2");
        assert_eq!(store.verify_disk_chain(sid), SourceChain::Verified);
        let log = root.join(sid.to_string()).join("log.jsonl");
        let body = std::fs::read_to_string(&log).expect("read");
        std::fs::write(&log, body.replacen("orig", "TAMPERED", 1)).expect("write");
        match store.verify_disk_chain(sid) {
            SourceChain::Unverified { at_index, .. } => assert_eq!(at_index, 1),
            other => panic!("tamper must break the chain: {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// A line that fails to parse (corrupt JSON) yields Unverified at that
    /// index, not a panic -- the verify is best-effort.
    #[tokio::test]
    async fn test_corrupt_line_unverified() {
        let root = temp_root();
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        let sid = SessionId::new();
        store
            .append(ev(sid, TurnEventKind::UserInput { text: "ok".into() }))
            .await
            .expect("append");
        // Append a garbage line after the valid one.
        let log = root.join(sid.to_string()).join("log.jsonl");
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"not-json\n").unwrap();
        match store.verify_disk_chain(sid) {
            SourceChain::Unverified { .. } => {}
            other => panic!("a corrupt line must yield Unverified, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
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
                TurnEventKind::UserInput {
                    text: "seeded".into(),
                },
                TurnEventKind::AssistantMessage {
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
                TurnEventKind::UserInput {
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
        std::fs::remove_dir_all(&root).ok();
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
        vec![TurnEventKind::UserInput {
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
            e.kind,
            TurnEventKind::UserInput { ref text } if text == "A's prompt"
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
        vec![TurnEventKind::UserInput {
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
        .append(TurnEvent {
            id: EventId::new(),
            session: sid_b,
            ts: 1,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
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
        .map(|e| match &e.kind {
            TurnEventKind::UserInput { text } => text.as_str(),
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
    let root = std::env::temp_dir().join(format!("child-result-unit-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("mkdir");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let child = SessionId::new();
    store
        .append(evt(
            child,
            EventId::new(),
            TurnEventKind::UserInput { text: "go".into() },
        ))
        .await
        .expect("append");
    let result = store.read_child_result(child);
    assert!(!result.is_empty(), "child result should have events");
    assert!(store.read_child_result(SessionId::new()).is_empty());
    std::fs::remove_dir_all(&root).ok();
}

/// Cold prev_hash (cache miss) hashes the raw last disk line via reverse-read,
/// not a re-serialization of the replayed last event. Uses a LocalFileBackend
/// so the reverse-read path is real (InMemoryBackend returns empty, hitting
/// the fallback).
#[tokio::test]
async fn test_prev_hash_reads_raw() {
    let root = std::env::temp_dir().join(format!(
        "cold-prev-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    drop(appended_event(&store, sid, TurnEventKind::UserInput { text: "a".into() }).await);
    let e2 = appended_event(
        &store,
        sid,
        TurnEventKind::AssistantMessage {
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
    std::fs::remove_dir_all(&root).ok();
}

/// The cold prev_hash stays byte-stable under a serde schema drift: a line
/// carrying an unknown field (a prior binary wrote it) parses, but
/// re-serialization omits the field and drifts. The cold path must hash the
/// raw line bytes, not the re-serialized reparsed event.
#[tokio::test]
async fn test_prev_hash_survives_drift() {
    let root = std::env::temp_dir().join(format!(
        "cold-drift-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    drop(appended_event(&store, sid, TurnEventKind::UserInput { text: "a".into() }).await);
    let rr = store.backend().read_lines_reverse(sid, u64::MAX, 1_048_576);
    let orig_line = rr.lines.first().expect("last line").1.clone();
    // Insert an unknown field a prior binary's schema carried; the current
    // schema ignores it on parse, re-serialization omits it.
    let brace = orig_line.rfind('}').unwrap();
    let drifted_line = format!("{},\"zz_future_drift\":0}}", &orig_line[..brace]);
    let log_path = root.join(sid.to_string()).join("log.jsonl");
    std::fs::write(&log_path, format!("{drifted_line}\n")).unwrap();
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
    std::fs::remove_dir_all(&root).ok();
}
