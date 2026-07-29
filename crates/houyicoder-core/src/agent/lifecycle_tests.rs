use super::*;

/// extract_precompact_markers finds an unsolved-problem marker (error) in a
/// Summarized assistant message and builds a stable key.
#[tokio::test]
async fn test_markers_find_unsolved() {
    use houyicoder_context::{CheckpointId, Disposition};
    let s = SessionId::new();
    let id1 = EventId::new();
    let id2 = EventId::new();
    let id3 = EventId::new();
    let events = vec![
        TurnEvent {
            id: id1,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "hit an error here".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: id2,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "we decided to use rust".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: id3,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "latest".into(),
                thinking: None,
            },
        },
    ];
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session: s,
        last_event: id3,
        summary: None,
        plan: vec![
            houyicoder_context::TurnGroup {
                turn_id: id1,
                disposition: Disposition::Summarized,
                event_ids: vec![id1, id2],
            },
            houyicoder_context::TurnGroup {
                turn_id: id3,
                disposition: Disposition::Verbatim,
                event_ids: vec![id3],
            },
        ],
        ts: 0,
    };
    let markers = extract_precompact_markers(&events, &manifest);
    assert!(
        markers.len() >= 2,
        "both markers found: {:?}",
        markers.iter().map(|m| &m.key).collect::<Vec<_>>()
    );
    assert!(
        markers
            .iter()
            .any(|m| m.key.starts_with("compact-unsolved")),
        "unsolved marker present"
    );
    assert!(
        markers
            .iter()
            .any(|m| m.key.starts_with("compact-decision")),
        "decision marker present"
    );
    // The Verbatim event (id3, "latest") is NOT scanned.
    assert!(
        markers.iter().all(|m| m.content != "latest"),
        "verbatim event not scanned"
    );
}

/// extract_preclear_markers scans EVERY model-visible event (no manifest
/// filter) — /clear drops the whole session, so the entire stream is the
/// about-to-drop span. The same marker phrases + key derivation as the
/// compact path are reused.
#[tokio::test]
async fn test_preclear_scans_all_events() {
    let s = SessionId::new();
    let id1 = EventId::new();
    let id2 = EventId::new();
    let id3 = EventId::new();
    let events = vec![
        TurnEvent {
            id: id1,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "hit an error here".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: id2,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "we decided to use rust".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: id3,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "latest".into(),
            },
        },
    ];
    let markers = extract_preclear_markers(&events);
    assert!(
        markers.len() >= 2,
        "both markers found across all events: {:?}",
        markers.iter().map(|m| &m.key).collect::<Vec<_>>()
    );
    assert!(
        markers
            .iter()
            .any(|m| m.key.starts_with("compact-unsolved")),
        "unsolved marker present"
    );
    assert!(
        markers
            .iter()
            .any(|m| m.key.starts_with("compact-decision")),
        "decision marker present"
    );
    // Unlike compact, clear scans the latest event too — a marker in the
    // verbatim tail survives the clear. (id3 "latest" has no marker, but a
    // marker there would be caught.)
    assert!(
        markers.iter().all(|m| m.content != "latest"),
        "non-marker text not extracted as a marker"
    );
}

/// Same content + kind yields the same key (dedup-stable).
#[tokio::test]
async fn test_markers_dedup_stable() {
    use houyicoder_context::{CheckpointId, Disposition};
    let s = SessionId::new();
    let id1 = EventId::new();
    let id2 = EventId::new();
    let text = "the build is broken again";
    let events = vec![
        TurnEvent {
            id: id1,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: text.into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: id2,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: text.into(),
                thinking: None,
            },
        },
    ];
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session: s,
        last_event: id2,
        summary: None,
        plan: vec![houyicoder_context::TurnGroup {
            turn_id: id1,
            disposition: Disposition::Summarized,
            event_ids: vec![id1, id2],
        }],
        ts: 0,
    };
    let markers = extract_precompact_markers(&events, &manifest);
    // Two hits (one per event) but both have the same key (same content).
    let keys: Vec<&str> = markers.iter().map(|m| m.key.as_str()).collect();
    assert_eq!(keys[0], keys[1], "same content → same key for dedup");
}

/// find_markers emits one marker per kind per text, not one per matching
/// phrase. A text with both "error" and "broken" (two unsolved phrases)
/// yields a single UnsolvedProblem marker — the key is kind plus a content
/// slug, so a second same-kind marker would collide on the key + the
/// caller's add overwrites (losing the first phrase's meta).
#[test]
fn test_find_markers_dedups_kind() {
    let markers = find_markers("hit an error and it's broken");
    let unsolved = markers
        .iter()
        .filter(|m| matches!(m.kind, MarkerKind::UnsolvedProblem))
        .count();
    assert_eq!(unsolved, 1, "one unsolved marker per text, not per phrase");
    // A text with an unsolved + a decision phrase yields one of each.
    let markers = find_markers("hit an error, decided to retry");
    assert_eq!(markers.len(), 2, "one unsolved + one decision");
}

/// Runner::compact drives a full manual compaction: replay, build a manifest
/// with the default policy + the runner's summarizer, persist it. The /compact
/// command calls this. Pins the core API the TUI dispatches — that the manifest
/// is written, the summary is populated, and the folded count is reported.
#[tokio::test]
async fn test_compact_persists_manifest() {
    use crate::agent::Runner;
    use crate::provider::test_support::FakeProvider;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_session::SessionStore;
    let s = SessionId::new();
    let ids: Vec<EventId> = (0..6).map(|_| EventId::new()).collect();
    let events = vec![
        TurnEvent {
            id: ids[0],
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "do work".into(),
            },
        },
        TurnEvent {
            id: ids[1],
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "old".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: ids[2],
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "mid1".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: ids[3],
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "mid2".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: ids[4],
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "recent".into(),
                thinking: None,
            },
        },
        TurnEvent {
            id: ids[5],
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "latest".into(),
                thinking: None,
            },
        },
    ];
    let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    for ev in &events {
        store.append(ev.clone()).await.unwrap();
    }
    let runner = Runner::new(
        store.clone(),
        std::sync::Arc::new(FakeProvider::text("summary")),
        crate::agent::tool::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            max_turns: 5,
            ..crate::agent::runner_config::RunnerConfig::default()
        },
    );
    let result = runner.compact(s).await.expect("compact runs");
    assert!(result.folded_count > 0, "older turns folded");
    // The manifest is persisted: a checkpoint is now listed for the session.
    let checkpoints = store.list_checkpoints(s).await.unwrap();
    assert_eq!(checkpoints.len(), 1, "compact wrote one checkpoint");
    let manifest = store.read_checkpoint(checkpoints[0]).await.unwrap();
    assert!(manifest.summary.is_some(), "manifest carries a summary");
    assert!(
        manifest.last_event == ids[5],
        "manifest covers through the last event"
    );
}
