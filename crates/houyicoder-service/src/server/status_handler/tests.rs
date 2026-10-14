//! Tests for session title derivation from the log head.

use super::*;
use houyicoder_api::session::SessionLog;
use houyicoder_context::SessionId;
use houyicoder_context::{EventId, SessionEvent, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::process::id;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Compact title formatting turns a prompt into a clean session name.
#[test]
fn test_compact_session_title() {
    assert_eq!(compact_session_title("Fix login bug"), "fix-login-bug");
    assert_eq!(
        compact_session_title("  Refactor  the  spec  strip  "),
        "refactor-the-spec-strip"
    );
    let long = compact_session_title("a".repeat(80).as_str());
    assert_eq!(long.chars().count(), 40);
    assert!(
        long.ends_with('\u{2026}'),
        "truncated title ends with ellipsis: {long}"
    );
}

#[tokio::test]
async fn test_first_prompt_title_log() {
    let root = temp_dir().join(format!(
        "title-test-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    store
        .append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "research demo repo".into(),
            },
        })
        .await
        .unwrap();
    let log: &dyn SessionLog = &store as &dyn SessionLog;
    let title = first_prompt_title(log, sid);
    assert_eq!(title.as_deref(), Some("research-demo-repo"));
    remove_dir_all(&root).ok();
}

#[tokio::test]
async fn test_first_prompt_title_absent() {
    let root = temp_dir().join(format!(
        "title-empty-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let sid = SessionId::new();
    let log: &dyn SessionLog = &store as &dyn SessionLog;
    assert!(first_prompt_title(log, sid).is_none());
    remove_dir_all(&root).ok();
}
