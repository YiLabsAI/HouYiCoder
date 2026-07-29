//! Server-side queue lifecycle tests: the invariant that the host's pending
//! queue is the single truth source + the server's queued_input is only the
//! current run's injection buffer. An interrupt or a state-changing command
//! invalidates the buffer, so orphaned texts must not leak into the next run.
//! These are the engine-level proofs; the wire-level coverage lives in the
//! service session tests.
use super::*;

/// A message enqueued after the run's turn-1 drain + then interrupted must
/// NOT survive in the server queue. Without the clear-on-interrupt, the
/// orphaned text sits in queued_input; the next run's turn-2 drain injects
/// it as a user message the user never re-sent (the very behavior the park
/// gate forbids on the host side, defeated by the server half the host gate
/// never sees).
#[tokio::test]
async fn test_interrupt_clears_queued_input() {
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::TextDelta {
        id: "t1".into(),
        text: "partial".into(),
    }]));
    let runner = Arc::new(runner_with(p, ToolRegistry::new()));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "go".into()).await });
    // Let the run enter the streaming select! past the turn-1 queue drain
    // (the drain runs before model_call_stream; the stream then emits the
    // delta + goes pending). Anything enqueued now lands AFTER that drain,
    // so only a turn-2 drain (which never happens) would consume it.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    runner.enqueue_input("m1".into());
    runner.abort();
    let result = task.await.expect("run task").expect("run ok");
    assert!(
        matches!(result.outcome, RunOutcome::Interrupted(_)),
        "expected Interrupted, got {:?}",
        result.outcome
    );
    // The orphan must be cleared: the next run must not re-inject m1.
    assert!(
        runner.queued_input_snapshot().is_empty(),
        "interrupted run must clear the server queue (single truth source is \
         the host); orphan found: {:?}",
        runner.queued_input_snapshot()
    );
}
