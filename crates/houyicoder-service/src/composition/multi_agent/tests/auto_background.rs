//! The foreground wait budget: when a slow delegation is sent to the
//! background, and when a fast one is not.

use super::*;

/// A foreground delegation that outlives its wait budget is sent to the
/// background and the turn is released: the outcome tells the model not to
/// guess the unfinished work, and the child keeps running so its ending
/// arrives later as a notification.
#[tokio::test]
async fn test_slow_child_auto_backgrounds() {
    let (provider, entered) = StallForeverProvider::new();
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let runtime = runtime.with_delegation_wait_ms(150);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "slow task", "slow task");

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert!(
        outcome.status.is_none(),
        "the turn is released without a terminal status: {:?}",
        outcome.status
    );
    let summary = outcome
        .summary
        .expect("the outcome carries the instruction");
    assert!(
        summary.contains("now running in the background"),
        "the model is told the child is in the background: {summary}"
    );
    assert!(
        summary.contains(&outcome.child_session_id),
        "the instruction names the child so the model can steer or resume it: {summary}"
    );
    entered.await.expect("the child entered its model call");
    runtime.kill_child(&outcome.child_session_id);
    runtime.join_children().await;
}

/// A foreground delegation that finishes inside its wait budget returns the
/// child's own ending, not a background launch, so the turn keeps the answer
/// it already paid for.
#[tokio::test]
async fn test_fast_child_keeps_foreground() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("the answer");
    let runtime = runtime.with_delegation_wait_ms(5_000);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "fast task", "fast task");

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert_eq!(
        outcome.status.as_deref(),
        Some("completed"),
        "a child that finishes inside the budget keeps its own ending"
    );
    assert_eq!(
        outcome.summary.as_deref(),
        Some("the answer"),
        "the turn keeps the child's answer, not a background instruction"
    );
}
