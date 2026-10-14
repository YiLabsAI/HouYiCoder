//! Tests for this module.

use super::{event_name, hex_short};
use houyicoder_context::SessionEvent;

#[test]
fn test_hex_short_first_bytes() {
    assert_eq!(hex_short(&[0xde, 0xad, 0xbe, 0xef, 0x42]), "deadbeef");
    assert_eq!(hex_short(&[]), "");
    assert_eq!(hex_short(&[0x00, 0xff]), "00ff");
}

/// An Unknown kind (a future-binary event type) labels as "unknown" so
/// the trajectory row does not mislead by borrowing another kind's label.
#[test]
fn test_unknown_kind_labeled_unknown() {
    assert_eq!(event_name(&SessionEvent::Unknown), "unknown");
}

#[test]
fn test_subagent_kinds_labeled() {
    assert_eq!(
        event_name(&SessionEvent::SubagentSpawn {
            child_session_id: String::new(),
            subagent_type: String::new(),
            prompt_summary: String::new(),
            isolation: String::new(),
            policy: String::new(),
            trigger_source: String::new(),
        }),
        "spawn"
    );
    assert_eq!(
        event_name(&SessionEvent::SubagentReturn {
            child_session_id: String::new(),
            status: String::new(),
            summary: String::new(),
            result_ref: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        "return"
    );
    assert_eq!(
        event_name(&SessionEvent::ChildDelegated {
            parent_session_id: String::new(),
            subagent_type: String::new(),
        }),
        "delegated",
        "the child's own record of the delegation is an audit row"
    );
    assert_eq!(
        event_name(&SessionEvent::NotificationInjected {
            child_session_id: String::new(),
            turn: 0,
            order: 0,
            topic: String::new(),
            summary: String::new(),
        }),
        "notify"
    );
}
