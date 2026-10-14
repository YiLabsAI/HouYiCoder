use super::*;
use houyicoder_protocol::frontend::hooks::HookEntry;

fn fw(name: &str) -> HookEntry {
    HookEntry {
        name: name.into(),
        events: vec![name.into()],
        source: "framework".into(),
        fired: false,
        summary: "event def".into(),
        description: String::new(),
    }
}

fn hook(event: &str, source: &str) -> HookEntry {
    HookEntry {
        name: format!("hook-for-{event}"),
        events: vec![event.into()],
        source: source.into(),
        fired: false,
        summary: String::new(),
        description: String::new(),
    }
}

#[test]
fn test_count_zero_no_hooks() {
    let entries = vec![fw("PreToolUse"), fw("PostToolUse")];
    assert_eq!(configured_count(&entries, "PreToolUse"), 0);
}

#[test]
fn test_count_one_match() {
    let entries = vec![fw("PreToolUse"), hook("PreToolUse", "user")];
    assert_eq!(configured_count(&entries, "PreToolUse"), 1);
}

#[test]
fn test_count_skips_framework() {
    let entries = vec![fw("PreToolUse"), hook("PreToolUse", "framework")];
    assert_eq!(configured_count(&entries, "PreToolUse"), 0);
}

#[test]
fn test_total_non_framework() {
    let entries = vec![
        fw("PreToolUse"),
        hook("PreToolUse", "user"),
        hook("PostToolUse", "plugin"),
        fw("PostToolUse"),
    ];
    assert_eq!(total_configured(&entries), 2);
}
