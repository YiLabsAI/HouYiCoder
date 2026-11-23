//! The fleet stop reaching a process child.

use super::*;

/// A process child has no runner handle, so both stop verbs must reach the
/// token it filed when it started; otherwise the fleet could not end it.
#[test]
fn test_external_child_stop() {
    let (runtime, _store, _parent_sid) = runtime_with_text_child("ok");
    let stop = CancellationToken::new();
    runtime.register_external_child("acp-c1".into(), stop.clone());
    assert!(
        runtime.kill_child("acp-c1"),
        "the fleet's stop reaches a process child"
    );
    assert!(stop.is_cancelled(), "the child's own token fired");
    let turn = CancellationToken::new();
    runtime.register_external_child("acp-c2".into(), turn.clone());
    assert!(
        runtime.cancel_child_turn("acp-c2"),
        "a turn stop on a process child ends it too"
    );
    assert!(turn.is_cancelled(), "the second child's token fired");
    runtime.unregister_external_child("acp-c1");
    assert!(!runtime.kill_child("acp-c1"), "a gone child returns false");
}
