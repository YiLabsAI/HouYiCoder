//! Tests for this module.

use super::HookEvent;

#[test]
fn test_all_events_have_description() {
    for e in HookEvent::ALL {
        let d = e.description();
        assert!(!d.is_empty(), "{e:?} has empty description");
        assert!(
            d.contains("Input is JSON"),
            "{e:?} description should name the input format"
        );
    }
}

#[test]
fn test_blocking_events_mention_deny() {
    for e in [
        HookEvent::PreToolUse,
        HookEvent::UserPromptSubmit,
        HookEvent::ConfigChange,
    ] {
        assert!(
            e.description().contains("Deny"),
            "{e:?} should mention Deny in its description"
        );
    }
}

#[test]
fn test_observability_only_events() {
    for e in [
        HookEvent::PostToolUseFailure,
        HookEvent::Notification,
        HookEvent::SessionEnd,
    ] {
        assert!(
            e.description().contains("Observability only"),
            "{e:?} should be observability only"
        );
    }
}
