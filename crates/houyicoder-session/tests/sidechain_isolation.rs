//! Sidechain isolation: a child session's log is independent of the parent's.
//! The parent's log carries the spawn + return boundaries (durable, replay
//! reconstructs them); the child's turns live in the child's own log. Neither
//! leaks into the other. This is the end-to-end proof the SessionStore routing
//! by SessionId gives true isolation, not just "the events exist somewhere."

#![allow(clippy::unwrap_in_result)]

use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_root() -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("sidechain-{}-{n}", std::process::id()));
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

/// Parent log carries spawn + return boundaries and replays them; child's
/// turn work does NOT leak into the parent log; child log carries its own
/// turns and does NOT see the parent's spawn/return events.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_sidechain_isolates_sessions() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let parent = SessionId::new();
    let child = SessionId::new();

    // Parent log: a prompt, a spawn boundary, a return, a follow-up.
    store
        .append(event(
            parent,
            TurnEventKind::UserInput {
                text: "delegate search".into(),
            },
        ))
        .await
        .expect("append parent 1");
    store
        .append(event(
            parent,
            TurnEventKind::SubagentSpawn {
                child_session_id: child.to_string(),
                subagent_type: "explore".into(),
                prompt_summary: "find the auth module".into(),
                isolation: "worktree".into(),
                policy: "delegate".into(),
                trigger_source: "model:call-1".into(),
            },
        ))
        .await
        .expect("append spawn");
    store
        .append(event(
            parent,
            TurnEventKind::SubagentReturn {
                child_session_id: child.to_string(),
                status: "completed".into(),
                summary: "auth lives in crates/api".into(),
                result_ref: "evt-42".into(),
                input_tokens: 800,
                output_tokens: 400,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            },
        ))
        .await
        .expect("append return");
    store
        .append(event(
            parent,
            TurnEventKind::UserInput {
                text: "now write tests".into(),
            },
        ))
        .await
        .expect("append parent 2");

    // Child log: its own turn work — invisible to the parent.
    store
        .append(event(
            child,
            TurnEventKind::UserInput {
                text: "search auth".into(),
            },
        ))
        .await
        .expect("append child 1");
    store
        .append(event(
            child,
            TurnEventKind::AssistantMessage {
                text: "found auth in crates/api/src/auth.rs".into(),
                thinking: None,
            },
        ))
        .await
        .expect("append child 2");

    // Parent log: read from DISK (not the in-memory mirror) so the
    // assertion proves routing-level isolation, not just HashMap partition.
    let parent_events = store
        .backend()
        .read_log(parent)
        .expect("read parent log from disk");
    assert!(
        parent_events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. })),
        "parent log must carry the spawn boundary:\n{:?}",
        parent_events
    );
    assert!(
        parent_events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentReturn { .. })),
        "parent log must carry the return boundary:\n{:?}",
        parent_events
    );
    assert!(
        !parent_events.iter().any(|e| {
            matches!(
                &e.kind,
                TurnEventKind::AssistantMessage { text, .. }
                    if text.contains("found auth in crates/api")
            )
        }),
        "child turn text must not leak into parent log:\n{:?}",
        parent_events
    );

    // Child log: read from DISK too.
    let child_events = store
        .backend()
        .read_log(child)
        .expect("read child log from disk");
    assert!(
        child_events.iter().any(|e| {
            matches!(
                &e.kind,
                TurnEventKind::AssistantMessage { text, .. }
                    if text.contains("found auth in crates/api")
            )
        }),
        "child log must carry its own assistant reply:\n{:?}",
        child_events
    );
    assert!(
        !child_events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. })),
        "parent spawn must not leak into child log:\n{:?}",
        child_events
    );
    assert!(
        !child_events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentReturn { .. })),
        "parent return must not leak into child log:\n{:?}",
        child_events
    );

    std::fs::remove_dir_all(&root).ok();
}
