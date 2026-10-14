//! Tests for this module.

use super::*;
use crate::agent::hook::{HookSource, HookVerdict};

struct StubHook {
    name: String,
    events: Vec<HookEvent>,
    source: HookSource,
}

impl Hook for StubHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(HookVerdict::Allow)
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
}

#[test]
fn test_list_returns_registered_hooks() {
    let reg = HookRegistry::new();
    reg.register(Arc::new(StubHook {
        name: "pre-check".into(),
        events: vec![HookEvent::PreToolUse, HookEvent::PostToolUse],
        source: HookSource::Project,
    }));
    let entries = reg.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "pre-check");
    assert_eq!(
        entries[0].events,
        vec![HookEvent::PreToolUse, HookEvent::PostToolUse]
    );
    assert_eq!(entries[0].source, HookSource::Project);
}

#[test]
fn test_list_empty_registry() {
    let reg = HookRegistry::new();
    assert!(reg.list().is_empty());
}

/// The motivating use case for &self register: register on a
/// shared Arc<HookRegistry> after construction, then dispatch
/// and verify the hook fires. Proves the interior-mutability
/// change works end-to-end on a shared reference.
#[test]
fn test_register_on_shared_arc() {
    let reg = Arc::new(HookRegistry::new());
    // Register on the shared Arc — this is the &self path.
    reg.register(Arc::new(StubHook {
        name: "late-hook".into(),
        events: vec![HookEvent::PostToolUse],
        source: HookSource::User,
    }));
    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "bash".into(),
            input: serde_json::json!({}),
            result: ToolResult {
                output: "{}".into(),
            },
        },
        session: SessionId::new(),
    };
    let outcomes = reg.dispatch(&ctx);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].hook_name, "late-hook");
}

/// unregister removes a hook by its stable HookId: dispatch no longer
/// fires it, list drops it, and a sibling hook on the same event still
/// fires (the by_event cleanup filters one id, not the whole event).
/// An unknown id returns false without touching the registry. The
/// once:true skill-hook pattern relies on this to drop itself.
#[test]
fn test_unregister_removes_hook() {
    let reg = HookRegistry::new();
    let keep_id = reg.register(Arc::new(StubHook {
        name: "keep".into(),
        events: vec![HookEvent::PostToolUse],
        source: HookSource::User,
    }));
    let drop_id = reg.register(Arc::new(StubHook {
        name: "drop".into(),
        events: vec![HookEvent::PostToolUse],
        source: HookSource::User,
    }));
    // Both fire before unregister.
    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "bash".into(),
            input: serde_json::json!({}),
            result: ToolResult {
                output: "{}".into(),
            },
        },
        session: SessionId::new(),
    };
    assert_eq!(reg.dispatch(&ctx).len(), 2);
    // Unregister "drop"; "keep" stays. Unknown id returns false.
    assert!(reg.unregister(drop_id), "unregister a live id returns true");
    assert!(
        !reg.unregister(HookId(u64::MAX)),
        "unregister an unknown id returns false"
    );
    let outcomes = reg.dispatch(&ctx);
    assert_eq!(outcomes.len(), 1, "only the kept hook fires");
    assert_eq!(outcomes[0].hook_name, "keep");
    let names: Vec<String> = reg.list().iter().map(|e| e.name.clone()).collect();
    assert_eq!(
        names,
        vec!["keep".to_string()],
        "list drops the unregistered hook"
    );
    // The kept id is still valid (unregister did not over-remove).
    assert!(reg.unregister(keep_id), "the kept id is still live");
    assert_eq!(
        reg.dispatch(&ctx).len(),
        0,
        "no hooks fire after both removed"
    );
}

/// HookId is not reused: after unregister, a fresh register mints a
/// new id, so a stale handle held by a caller cannot accidentally
/// target a later-registered hook.
#[test]
fn test_hook_id_not_reused() {
    let reg = HookRegistry::new();
    let first = reg.register(Arc::new(StubHook {
        name: "first".into(),
        events: vec![HookEvent::PreToolUse],
        source: HookSource::User,
    }));
    assert!(reg.unregister(first));
    let second = reg.register(Arc::new(StubHook {
        name: "second".into(),
        events: vec![HookEvent::PreToolUse],
        source: HookSource::User,
    }));
    assert_ne!(first, second, "a new registration mints a fresh id");
    assert!(
        !reg.unregister(first),
        "the stale handle does not target the new hook"
    );
}
