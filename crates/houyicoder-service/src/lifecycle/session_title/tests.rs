//! Tests for the title read from a log head.

use super::log_head_title;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::PathBuf;
use std::process::id;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = temp_dir().join(format!("session-title-{tag}-{}-{nanos}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("mkdir root");
    root
}

fn entry(sid: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event,
    }
}

async fn append(store: &SessionStore, sid: SessionId, text: &str) {
    store
        .append(entry(sid, SessionEvent::UserInput { text: text.into() }))
        .await
        .expect("append");
}

/// A greeting opens a session without saying what the work is, so the title
/// search keeps reading until a prompt states one.
#[tokio::test]
async fn test_title_skips_greeting() {
    let root = temp_root("greeting");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append(&store, sid, "你好").await;
    append(&store, sid, "research demo repo").await;
    let log: &dyn SessionLog = &store;
    assert_eq!(
        log_head_title(log, sid).as_deref(),
        Some("research demo repo"),
        "the first statement that names the work is the title"
    );
    remove_dir_all(&root).ok();
}

/// A queued interjection is human input too: a session that opens with a
/// greeting and states its work in the first interjection is titled by that
/// statement, the same way the descriptor recorder judges both kinds.
#[tokio::test]
async fn test_title_reads_interjection() {
    let root = temp_root("interjection");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append(&store, sid, "你好").await;
    store
        .append(entry(
            sid,
            SessionEvent::MidTurnInput {
                text: "rework the retry budget".into(),
                pending_input_id: Some(7),
            },
        ))
        .await
        .expect("append interjection");
    let log: &dyn SessionLog = &store;
    assert_eq!(
        log_head_title(log, sid).as_deref(),
        Some("rework the retry budget"),
        "a statement queued mid-run names the session"
    );
    remove_dir_all(&root).ok();
}

/// An interjection carrying no queue identity came from another agent's
/// channel, not from the human, so it does not name the session.
#[tokio::test]
async fn test_title_skips_agent_message() {
    let root = temp_root("agent-message");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    append(&store, sid, "你好").await;
    store
        .append(entry(
            sid,
            SessionEvent::MidTurnInput {
                text: "child finished: tests pass".into(),
                pending_input_id: None,
            },
        ))
        .await
        .expect("append message");
    let log: &dyn SessionLog = &store;
    assert!(
        log_head_title(log, sid).is_none(),
        "another agent's words do not name the session"
    );
    remove_dir_all(&root).ok();
}

#[tokio::test]
async fn test_title_absent_without_log() {
    let root = temp_root("absent");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let log: &dyn SessionLog = &store;
    assert!(log_head_title(log, SessionId::new()).is_none());
    remove_dir_all(&root).ok();
}
