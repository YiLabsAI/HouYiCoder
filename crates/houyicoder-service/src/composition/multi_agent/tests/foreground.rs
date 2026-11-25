//! The foreground delegation path: the call returns at the terminal boundary
//! and the parent log records the pair replay needs.

use super::*;

#[tokio::test]
async fn test_foreground_spawn_reaches_terminal() {
    let (runtime, store, parent_sid) = runtime_with_text_child("child answer");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "find the auth module", "find auth");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    assert_eq!(outcome.summary.as_deref(), Some("child answer"));
    // The parent log carries the durable spawn + return boundary pair so
    // replay reconstructs the delegation.
    let events = store.trajectory_tail(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentSpawn { .. })),
        "parent log must record the SubagentSpawn boundary",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "parent log must record the SubagentReturn boundary",
    );
}
