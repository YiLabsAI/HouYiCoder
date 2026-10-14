//! Tests for this module.

use super::*;
use houyicoder_context::{Disposition, SessionEvent, SessionLogEntry};

fn event(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

#[test]
fn test_in_memory_append_replay() {
    let b = InMemoryBackend::new();
    let s = SessionId::new();
    let e1 = event(
        s,
        EventId::new(),
        SessionEvent::UserInput { text: "a".into() },
    );
    let e2 = event(
        s,
        EventId::new(),
        SessionEvent::AssistantMessage {
            text: "b".into(),
            thinking: None,
        },
    );
    pollster::block_on(b.append(e1.clone())).unwrap();
    pollster::block_on(b.append(e2.clone())).unwrap();
    let replay = pollster::block_on(b.replay(s)).unwrap();
    assert_eq!(replay.len(), 2);
    assert_eq!(replay[0].id, e1.id);
    assert_eq!(replay[1].id, e2.id);
}

#[test]
fn test_in_memory_dedups_duplicate() {
    let b = InMemoryBackend::new();
    let s = SessionId::new();
    let id = EventId::new();
    let e = event(s, id, SessionEvent::UserInput { text: "a".into() });
    pollster::block_on(b.append(e.clone())).unwrap();
    pollster::block_on(b.append(e.clone())).unwrap();
    let replay = pollster::block_on(b.replay(s)).unwrap();
    assert_eq!(replay.len(), 1, "duplicate id must be deduped");
}

#[test]
fn test_tool_pair_survives_replay() {
    let b = InMemoryBackend::new();
    let s = SessionId::new();
    let call_id = "toolu_call".to_string();
    pollster::block_on(b.append(event(
        s,
        EventId::new(),
        SessionEvent::ToolCall {
            call_id: call_id.clone(),
            tool: "edit".into(),
            input: serde_json::json!({}),
        },
    )))
    .unwrap();
    pollster::block_on(b.append(event(
        s,
        EventId::new(),
        SessionEvent::ToolResult {
            call_id,
            output: serde_json::json!("ok"),
            duration_ms: 0,
        },
    )))
    .unwrap();
    let replay = pollster::block_on(b.replay(s)).unwrap();
    assert_eq!(replay.len(), 2);
    assert!(matches!(replay[0].event, SessionEvent::ToolCall { .. }));
    assert!(matches!(replay[1].event, SessionEvent::ToolResult { .. }));
}

#[test]
fn test_in_memory_checkpoint_round() {
    let b = InMemoryBackend::new();
    let s = SessionId::new();
    let last = EventId::new();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session: s,
        last_event: last,
        summary: Some("summarized the head".into()),
        plan: vec![houyicoder_context::TurnGroup {
            turn_id: last,
            disposition: Disposition::Verbatim,
            event_ids: vec![last],
        }],
        ts: 0,
    };
    let id = pollster::block_on(b.write_checkpoint(manifest.clone()));
    assert_eq!(id.unwrap(), manifest.id);
    let back = pollster::block_on(b.read_checkpoint(manifest.id)).unwrap();
    assert_eq!(back.summary, manifest.summary);
    let list = pollster::block_on(b.list_checkpoints(s)).unwrap();
    assert_eq!(list, vec![manifest.id]);
}

#[test]
fn test_block_put_get_roundtrip() {
    let b = InMemoryBackend::new();
    let blob = b"hello cas block store".to_vec();
    let hash = pollster::block_on(b.block_put(blob.clone())).unwrap();
    let back = pollster::block_on(b.block_get(&hash)).unwrap();
    assert_eq!(back, blob);
}

#[test]
fn test_block_put_dedup_same() {
    let b = InMemoryBackend::new();
    let blob = b"dedup me".to_vec();
    let h1 = pollster::block_on(b.block_put(blob.clone())).unwrap();
    let h2 = pollster::block_on(b.block_put(blob.clone())).unwrap();
    assert_eq!(h1, h2, "same content must yield same hash");
    // Retrieval still works after duplicate put.
    let back = pollster::block_on(b.block_get(&h1)).unwrap();
    assert_eq!(back, blob);
}

#[test]
fn test_block_put_different_content() {
    let b = InMemoryBackend::new();
    let h1 = pollster::block_on(b.block_put(b"one".to_vec())).unwrap();
    let h2 = pollster::block_on(b.block_put(b"two".to_vec())).unwrap();
    assert_ne!(h1, h2, "different content must yield different hash");
}

#[test]
fn test_block_get_missing() {
    let b = InMemoryBackend::new();
    let res = pollster::block_on(b.block_get(&BlockHash("0".repeat(64))));
    assert!(matches!(res, Err(ContextError::NotFound)));
}

/// Regression for the search snapshot: read_log must return all events
/// for a session so the search view's snapshot seam can load the full
/// transcript. Before the fix, InMemoryBackend did not implement
/// read_log, so the trait default returned Err(Unsupported) -> the
/// lenient read returned empty -> /search found nothing. The lenient
/// read delegates to read_log (no corrupt lines in memory).
#[test]
fn test_read_log_returns_events() {
    let b = InMemoryBackend::new();
    let s = SessionId::new();
    // Cold session: empty.
    assert!(b.read_log(s).unwrap().is_empty());
    // Append two events.
    pollster::block_on(b.append(event(
        s,
        EventId::new(),
        SessionEvent::UserInput {
            text: "hello".into(),
        },
    )))
    .unwrap();
    pollster::block_on(b.append(event(
        s,
        EventId::new(),
        SessionEvent::UserInput {
            text: "world".into(),
        },
    )))
    .unwrap();
    // read_log returns all events in append order.
    let events = b.read_log(s).unwrap();
    assert_eq!(events.len(), 2, "read_log returns all appended events");
    // read_log_lenient delegates to read_log (default: Ok -> events, 0 skipped).
    let lenient = b.read_log_lenient(s);
    assert_eq!(
        lenient.events.len(),
        2,
        "lenient read returns the same events"
    );
    assert_eq!(
        lenient.skipped, 0,
        "no corrupt lines in memory -> 0 skipped"
    );
}

#[test]
fn test_block_large_blob_externalized() {
    // A large blob goes to the CAS, not the event log: events stay empty.
    let b = InMemoryBackend::new();
    let big = vec![0xABu8; 4096];
    let hash = pollster::block_on(b.block_put(big.clone())).unwrap();
    let back = pollster::block_on(b.block_get(&hash)).unwrap();
    assert_eq!(back, big);
    let events = pollster::block_on(b.replay(SessionId::new())).unwrap();
    assert!(events.is_empty(), "CAS blob must not land in event log");
}

#[test]
fn test_context_backend_is_object() {
    // A future method addition must not silently break runtime dispatch.
    let _boxed: Box<dyn ContextBackend> = Box::new(InMemoryBackend::new());
}
