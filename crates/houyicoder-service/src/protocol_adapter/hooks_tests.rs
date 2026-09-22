//! Tests for hook entry mapping.

use super::*;
use houyicoder_core::agent::{HookEntry, HookEvent, HookSource};

#[test]
fn test_hook_entries_convert() {
    let entries = vec![HookEntry {
        name: "pre-check".into(),
        events: vec![HookEvent::PreToolUse],
        source: HookSource::Project,
    }];
    let entries = hook_entries(entries);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "pre-check");
    assert_eq!(entries[0].events, vec!["PreToolUse"]);
    assert_eq!(entries[0].source, "Project");
}

#[test]
fn test_hook_entries_empty() {
    assert!(hook_entries(Vec::new()).is_empty());
}

/// The framework event catalog lists all 28 declared events, marking the
/// seven live ones as fired (three tool-lifecycle plus four reserved
/// subagent and worktree events).
#[test]
fn test_declared_events_list_all() {
    let entries = declared_events();
    assert_eq!(entries.len(), 28, "all declared events listed");
    let live: Vec<_> = entries.iter().filter(|e| e.fired).collect();
    assert_eq!(live.len(), 7, "seven live events");
    assert!(
        entries
            .iter()
            .any(|e| e.name == "PreToolUse" && e.source == "framework")
    );
}
