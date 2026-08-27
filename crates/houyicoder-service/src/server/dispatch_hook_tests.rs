use super::*;
use houyicoder_core::agent::{HookEntry, HookEvent, HookSource};

#[test]
fn test_hooks_wire_converts_entries() {
    let entries = vec![HookEntry {
        name: "pre-check".into(),
        events: vec![HookEvent::PreToolUse],
        source: HookSource::Project,
    }];
    let wire = hooks_to_wire(entries);
    assert_eq!(wire.len(), 1);
    assert_eq!(wire[0].name, "pre-check");
    assert_eq!(wire[0].events, vec!["PreToolUse"]);
    assert_eq!(wire[0].source, "Project");
}

#[test]
fn test_hooks_to_wire_empty() {
    assert!(hooks_to_wire(Vec::new()).is_empty());
}

/// The framework event surface lists all 28 declared events, marking the
/// seven live ones as fired (three tool-lifecycle plus four reserved
/// subagent and worktree events).
#[test]
fn test_events_surface_lists_all() {
    let wire = hook_events_to_wire();
    assert_eq!(wire.len(), 28, "all declared events listed");
    let live: Vec<_> = wire.iter().filter(|e| e.fired).collect();
    assert_eq!(live.len(), 7, "seven live events");
    assert!(
        wire.iter()
            .any(|e| e.name == "PreToolUse" && e.source == "framework")
    );
}

/// Slugify kebab-cases a prompt to a compact session-name title.
#[test]
fn test_slugify_compacts_prompt() {
    assert_eq!(slugify("Fix login bug"), "fix-login-bug");
    assert_eq!(
        slugify("  Refactor  the  spec  strip  "),
        "refactor-the-spec-strip"
    );
    assert_eq!(slugify("a".repeat(80).as_str()).chars().count(), 40);
}

#[tokio::test]
async fn test_first_prompt_slug_log() {
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
    use houyicoder_session::SessionStore;
    let root = std::env::temp_dir().join(format!(
        "slug-test-{}-{}",
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
        .append(TurnEvent {
            id: EventId::new(),
            session: sid,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "research demo repo".into(),
            },
        })
        .await
        .unwrap();
    let log: &dyn houyicoder_api::session::SessionLog =
        &store as &dyn houyicoder_api::session::SessionLog;
    let slug = first_prompt_slug(log, sid);
    assert_eq!(slug.as_deref(), Some("research-demo-repo"));
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn test_prompt_slug_empty_log() {
    use houyicoder_context::SessionId;
    use houyicoder_session::SessionStore;
    let root = std::env::temp_dir().join(format!(
        "slug-empty-{}-{}",
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
    assert!(first_prompt_slug(log, sid).is_none());
    std::fs::remove_dir_all(&root).ok();
}
