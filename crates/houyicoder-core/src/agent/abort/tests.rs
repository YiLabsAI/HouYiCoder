//! Tests for this module.

use super::*;
use crate::agent::RunOutcome;
use crate::agent::ToolRegistry;
use crate::agent::runner_tests::runner_with;
use crate::provider::test_support::FakeProvider;
use houyicoder_context::SessionId;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use std::sync::Arc;

fn stub_runner() -> Runner {
    runner_with(
        Arc::new(FakeProvider::new(vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "test".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }])),
        ToolRegistry::new(),
    )
}

/// abort() sets the durable flag; resume() checks it via
/// aborted_short_circuit and short-circuits to Interrupted (after
/// reconciling orphan results) instead of re-entering drive_loop. This is
/// the path that makes a cancel issued during the ask-wait actually
/// surface — the cancel token alone cannot reach a paused run. abort() only
/// sets the durable flag when the run is paused (mark_paused), so an idle
/// or in-flight abort cannot poison a later resume.
#[tokio::test]
async fn test_resume_short_circuits_abort() {
    let runner = stub_runner();
    let session = SessionId::new();
    // Simulate a run paused on an Interruption (the state mark_paused is
    // normally set at the Interruption return inside run/resume).
    runner.mark_paused();
    runner.abort();
    assert!(runner.is_aborted(), "abort sets the flag when paused");
    let result = runner.resume(session, &[]).await.expect("resume");
    assert!(
        matches!(result.outcome, RunOutcome::Interrupted(_)),
        "an aborted resume short-circuits to Interrupted, got {:?}",
        result.outcome
    );
    assert!(!runner.is_aborted(), "take_aborted cleared the flag");
}

/// An abort while NOT paused (idle / run in flight / resume in flight) does
/// NOT set the durable flag — the token handles the in-flight case, and
/// setting the flag would only poison a later, unrelated resume.
#[tokio::test]
async fn test_not_paused_skips_flag() {
    let runner = stub_runner();
    // No mark_paused — simulate an idle or in-flight cancel.
    runner.abort();
    assert!(
        !runner.is_aborted(),
        "abort while not paused must not set the durable flag"
    );
}

/// A fresh run() clears a stale aborted flag + the paused flag from a
/// prior cancel so the next resume() on a later Interruption is not
/// falsely skipped.
#[tokio::test]
async fn test_run_clears_stale_flag() {
    let runner = stub_runner();
    runner.mark_paused();
    runner.abort();
    assert!(runner.is_aborted());
    drop(runner.run(SessionId::new(), "go".into()).await);
    assert!(!runner.is_aborted(), "run() clears a stale abort flag");
}
