//! Tests for this module.

use std::sync::Arc;

use super::*;
use crate::agent::manifest::{CompressPolicy, HeuristicSummarizer, build_manifest};
use houyicoder_context::{EventId, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;

fn ev(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserInput { text: text.into() }
}

fn assistant(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        text: text.into(),
        thinking: None,
    }
}

fn ids(n: usize) -> Vec<EventId> {
    (0..n).map(|_| EventId::new()).collect()
}

async fn build_and_record(
    store: &SessionStore,
    session: SessionId,
    events: &[SessionLogEntry],
    policy: &CompressPolicy,
) -> RecordedCompaction {
    let manifest = build_manifest(events, policy, &HeuristicSummarizer, None).await;
    // The counts are the boundary's own evidence, written verbatim, so a
    // distinctive pair proves the wiring without depending on what a
    // summarizer happens to produce.
    record_compaction(store, session, &manifest, 111, 222)
        .await
        .unwrap()
}

#[tokio::test]
async fn test_record_writes_checkpoint_events() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let s = SessionId::new();
    let ids = ids(4);
    let events = vec![
        ev(s, ids[0], user("do work")),
        ev(s, ids[1], assistant("old response")),
        ev(s, ids[2], assistant("middle")),
        ev(s, ids[3], assistant("latest")),
    ];
    for e in &events {
        store.append(e.clone()).await.unwrap();
    }
    let policy = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let result = build_and_record(&store, s, &events, &policy).await;
    assert!(result.made_progress, "must fold some events");
    assert!(result.folded_count > 0);
    let back = store.read_checkpoint(result.manifest.id).await.unwrap();
    assert_eq!(back.summary, result.manifest.summary);
    assert_eq!(back.plan.len(), result.manifest.plan.len());
    let replay = store.replay(s).await.unwrap();
    let boundary: Vec<(u64, u64)> = replay
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::CompactionBoundary {
                pre_tokens,
                post_tokens,
                ..
            } => Some((*pre_tokens, *post_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(
        boundary,
        vec![(111, 222)],
        "one boundary, carrying the fold"
    );
    let summary_count = replay
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::Summary { .. }))
        .count();
    assert_eq!(summary_count, 1, "one summary event");
}

#[tokio::test]
async fn test_no_progress_keeps_all() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let s = SessionId::new();
    let ids = ids(2);
    let events = vec![ev(s, ids[0], user("task")), ev(s, ids[1], assistant("a1"))];
    let policy = CompressPolicy {
        tail_turns: 4,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let result = build_and_record(&store, s, &events, &policy).await;
    assert!(!result.made_progress, "all verbatim = no progress");
    assert_eq!(result.folded_count, 0);
    assert!(result.manifest.summary.is_none());
}

#[tokio::test]
async fn test_record_empty_events() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let s = SessionId::new();
    let policy = CompressPolicy::default();
    let result = build_and_record(&store, s, &[], &policy).await;
    assert!(!result.made_progress);
    assert!(result.manifest.plan.is_empty());
    assert!(result.manifest.summary.is_none());
    let replay = store.replay(s).await.unwrap();
    assert!(replay.is_empty(), "no boundary/summary for empty events");
}

#[tokio::test]
async fn test_record_checkpoint_round_trips() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let s = SessionId::new();
    let ids = ids(5);
    let events = vec![
        ev(s, ids[0], user("start")),
        ev(s, ids[1], assistant("a1")),
        ev(s, ids[2], assistant("a2")),
        ev(s, ids[3], assistant("a3")),
        ev(s, ids[4], assistant("latest")),
    ];
    for e in &events {
        store.append(e.clone()).await.unwrap();
    }
    let policy = CompressPolicy {
        tail_turns: 1,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let result = build_and_record(&store, s, &events, &policy).await;
    let id = result.manifest.id;
    let back = store.read_checkpoint(id).await.unwrap();
    assert_eq!(back.id, id);
    assert_eq!(back.session, s);
    assert_eq!(back.summary, result.manifest.summary);
    assert_eq!(back.plan, result.manifest.plan);
    let list = store.list_checkpoints(s).await.unwrap();
    assert!(list.contains(&id));
}

mod pre_flight_threshold;
