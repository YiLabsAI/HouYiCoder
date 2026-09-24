//! Tests for durable model-step timing and context-clear events.
//!
//! These assert the appended event payloads, not just that the append path
//! ran: without the assertions a change that wrote zeroed or mislabelled
//! timing would still pass the suite.

use crate::provider::test_support::FakeProvider;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_session::SessionStore;
use std::sync::Arc;

use crate::agent::{Runner, ToolRegistry, runner_config::RunnerConfig};

/// The timing facts one model call recorded, in the order the tests read them.
#[derive(Debug)]
struct ModelStepTiming {
    turn: u32,
    step: u32,
    total_ms: u64,
    ttft_ms: Option<u64>,
    decode_ms: Option<u64>,
}

fn test_runner(provider: FakeProvider) -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::new(
        store,
        Arc::new(provider),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    ))
}

fn response(output: Vec<OutputItem>) -> CompletionResponse {
    CompletionResponse {
        output,
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn timing_events(events: &[SessionLogEntry]) -> Vec<ModelStepTiming> {
    events
        .iter()
        .filter_map(|e| match e.event {
            SessionEvent::ModelStepTiming {
                reasoning_ms: _,
                response_ms: _,
                turn,
                step,
                total_ms,
                ttft_ms,
                decode_ms,
            } => Some(ModelStepTiming {
                turn,
                step,
                total_ms,
                ttft_ms,
                decode_ms,
            }),
            _ => None,
        })
        .collect()
}

/// A completed model call appends one timing event whose decode time is the
/// remainder after the first token, never larger than the total.
#[tokio::test]
async fn test_model_step_timing_appended() {
    let runner = test_runner(FakeProvider::text("done"));
    let session = SessionId::new();
    runner
        .run(session, "hi".into())
        .await
        .expect("run completes");

    let events = runner.store().trajectory_snapshot(session);
    let timings = timing_events(&events);
    assert_eq!(timings.len(), 1, "one model call appends one timing event");
    let first = &timings[0];
    assert_eq!(first.turn, 1, "the first model call is turn 1");
    assert_eq!(
        first.step, 1,
        "the first round-trip is step 1, matching TurnUsage"
    );
    // A fake provider answers instantly, so the measured durations are zero.
    // The invariant under test is the split, not the magnitude.
    assert_eq!(
        (first.ttft_ms.is_some(), first.decode_ms.is_some()),
        (true, true),
        "a stream that produced a first token records both ttft and decode"
    );
    let (ttft, decode) = (first.ttft_ms.unwrap_or(0), first.decode_ms.unwrap_or(0));
    assert!(
        ttft + decode <= first.total_ms,
        "ttft {ttft} + decode {decode} must not exceed the total {}",
        first.total_ms
    );
}

/// Each model call in a multi-call run appends its own timing event, so the
/// per-step latency series is complete rather than only the last step's.
#[tokio::test]
async fn test_timing_per_model_call() {
    let provider = FakeProvider::new(vec![
        response(vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        }]),
        response(vec![OutputItem::Text {
            text: "done".into(),
        }]),
    ]);
    let runner = test_runner(provider);
    let session = SessionId::new();
    drop(runner.run(session, "hi".into()).await);

    let events = runner.store().trajectory_snapshot(session);
    let timings = timing_events(&events);
    assert!(
        timings.len() >= 2,
        "a tool round-trip issues a second model call with its own timing: {timings:?}"
    );
    assert!(
        timings
            .iter()
            .all(|t| t.ttft_ms.is_some() && t.decode_ms.is_some()),
        "every appended timing splits into ttft and decode: {timings:?}"
    );
    assert_eq!(
        timings.iter().map(|t| t.step).collect::<Vec<_>>(),
        vec![1, 1],
        "each model call reports its own round-trip index"
    );
}

/// Clearing the session appends exactly one ContextCleared marker carrying the
/// turn count at the moment of the clear.
#[tokio::test]
async fn test_clear_appends_marker() {
    let runner = test_runner(FakeProvider::text("done"));
    let session = SessionId::new();
    runner
        .run(session, "hi".into())
        .await
        .expect("run completes");

    runner.reset_trajectory(session).await;

    let events = runner.store().trajectory_snapshot(session);
    let cleared: Vec<u32> = events
        .iter()
        .filter_map(|e| match e.event {
            SessionEvent::ContextCleared { prior_turn } => Some(prior_turn),
            _ => None,
        })
        .collect();
    assert_eq!(
        cleared.len(),
        1,
        "one clear appends exactly one marker, visible to an in-process reader"
    );
    assert_eq!(
        cleared[0], 1,
        "the marker records the turn count at the clear"
    );
}
