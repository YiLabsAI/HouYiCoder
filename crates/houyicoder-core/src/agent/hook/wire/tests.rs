//! Tests for this module.

use super::*;

/// The compact hooks wire to their HookEventKind variants. Pins the
/// PreCompact/PostCompress naming.
#[test]
fn test_compact_hooks_wire_correctly() {
    assert_eq!(
        wire_event_kind(HookEvent::PreCompact),
        HookEventKind::PreCompact
    );
    assert_eq!(
        wire_event_kind(HookEvent::PostCompact),
        HookEventKind::PostCompact
    );
}
