//! Cross-session result read: the parent reads a child's full transcript
//! via read_child_result. A deleted child log degrades to empty so the
//! caller falls back to the inline summary the parent already holds.

#![allow(clippy::unwrap_in_result)]

use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_root() -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("child-result-{}-{n}", std::process::id()));
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

/// read_child_result returns the child's full log; a non-existent child
/// degrades to empty (the caller falls back to the inline summary).
#[tokio::test]
async fn test_child_result_reads_log() {
    let root = temp_root();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let child = SessionId::new();

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
                text: "found auth in crates/api".into(),
                thinking: None,
            },
        ))
        .await
        .expect("append child 2");

    let result = store.read_child_result(child);
    assert_eq!(result.len(), 2, "child log should have 2 events");
    assert!(
        result.iter().any(|e| {
            matches!(
                &e.kind,
                TurnEventKind::AssistantMessage { text, .. }
                    if text.contains("found auth in crates/api")
            )
        }),
        "child result must carry the assistant reply:\n{:?}",
        result
    );

    let ghost = SessionId::new();
    let empty = store.read_child_result(ghost);
    assert!(
        empty.is_empty(),
        "missing child log should degrade to empty"
    );

    std::fs::remove_dir_all(&root).ok();
}
