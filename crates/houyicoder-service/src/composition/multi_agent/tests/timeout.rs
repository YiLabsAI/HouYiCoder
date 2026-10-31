//! What a delegation reports when its own ceiling ends the run.

use super::*;

/// A child that outlives its ceiling is stopped and reported as such.
///
/// The run used to end by dropping the driver, which dismantled the child
/// without running the tail that publishes its terminal, closes its inbox, and
/// releases the fence slot, and the parent got an error naming nothing the
/// child had done. The ceiling now stops the child the way a shutdown does and
/// reports the ending, so the parent reads a status it can act on.
#[tokio::test]
async fn test_ceiling_stops_child() {
    // A provider that pauses before every response, so the child cannot reach
    // a terminal of its own inside the ceiling below.
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new_with_delay(
        vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "unreachable".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }],
        5_000,
    ));
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.timeout_secs = Some(1);

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert_eq!(
        outcome.status.as_deref(),
        Some("timed_out"),
        "the ceiling is named as the thing that ended the run"
    );
    let summary = outcome.summary.unwrap_or_default();
    assert!(
        summary.contains("timed out after 1s"),
        "the report says how long it was given: {summary}"
    );
    assert!(
        summary.contains("resumable via"),
        "and says the work can be continued: {summary}"
    );
    assert!(
        summary.contains(&outcome.child_session_id),
        "naming the session that holds the work: {summary}"
    );
}

/// A child that finishes within its ceiling reports its own ending, so the
/// timeout path does not relabel work that completed.
#[tokio::test]
async fn test_ceiling_leaves_finished_alone() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("done"));
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.timeout_secs = Some(60);

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert_eq!(
        outcome.status.as_deref(),
        Some("completed"),
        "a run inside its ceiling keeps its own ending"
    );
    assert_eq!(outcome.summary.as_deref(), Some("done"));
}
