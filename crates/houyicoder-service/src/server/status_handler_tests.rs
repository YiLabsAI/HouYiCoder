//! Tests for session title derivation from the log head.

use super::*;

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
    use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
    use houyicoder_session::SessionStore;
    let root = std::env::temp_dir().join(format!(
        "title-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(houyicoder_memory::LocalFileBackend::new(
        root.clone(),
    )));
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
    let log: &dyn houyicoder_api::session::SessionLog =
        &store as &dyn houyicoder_api::session::SessionLog;
    let title = first_prompt_title(log, sid);
    assert_eq!(title.as_deref(), Some("research-demo-repo"));
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn test_first_prompt_title_absent() {
    use houyicoder_context::SessionId;
    use houyicoder_session::SessionStore;
    let root = std::env::temp_dir().join(format!(
        "title-empty-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = SessionStore::new(Box::new(houyicoder_memory::LocalFileBackend::new(
        root.clone(),
    )));
    let sid = SessionId::new();
    let log: &dyn houyicoder_api::session::SessionLog =
        &store as &dyn houyicoder_api::session::SessionLog;
    assert!(first_prompt_title(log, sid).is_none());
    std::fs::remove_dir_all(&root).ok();
}
