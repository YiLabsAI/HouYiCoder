//! Emit-wiring tests for the durable per-turn usage event.
//!
//! TurnUsage lands once per real provider round-trip (including length-
//! recovery retries, which burn real tokens), co-located with the in-memory
//! record_turn update. These tests pin the durable shape so resume / export
//! / the self-evolution re-reads see the real per-turn cost, not the
//! default-zero wire type.

use super::tests::length::ScriptRawProvider;
use super::tests::{HangingProvider, runner_with};
use super::*;
use crate::provider::test_support::FakeProvider;
use houyicoder_context::TurnEventKind;
use houyicoder_protocol::llm::{CompletionResponse, LlmEvent, OutputItem, Usage};

fn usage_response() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage {
            input_tokens: 1000,
            output_tokens: 500,
            total_tokens: 1500,
            non_cached_input_tokens: 150,
            cache_read_input_tokens: 800,
            cache_write_input_tokens: 50,
            reasoning_tokens: 100,
        },
        model: "stub-model".into(),
    }
}

#[tokio::test]
async fn test_turn_usage_records_cost() {
    let p = std::sync::Arc::new(FakeProvider::new(vec![usage_response()]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let events = runner.store().replay(session).await.expect("replay");
    let usage_ev = events
        .iter()
        .find(|e| matches!(e.kind, TurnEventKind::TurnUsage { .. }))
        .expect("a TurnUsage event lands per turn");
    match &usage_ev.kind {
        TurnEventKind::TurnUsage {
            turn,
            call_in_turn,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            model,
            recovery,
            ..
        } => {
            assert_eq!(*turn, 1, "one drive-loop turn => turn 1");
            assert_eq!(*call_in_turn, 1, "single call this turn");
            assert_eq!(*input_tokens, 1000);
            assert_eq!(*output_tokens, 500);
            assert_eq!(*cache_read_input_tokens, 800);
            assert_eq!(*cache_write_input_tokens, 50);
            assert_eq!(*reasoning_tokens, 100);
            assert_eq!(model, "test");
            // A non-retry terminal call: recovery false.
            assert!(!*recovery, "terminal call must not be flagged recovery");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn test_usage_one_per_turn() {
    // Two turns (tool call then final) => exactly two TurnUsage events, one
    // per model call. Guards against double-emit on the recovery path or a
    // missed emit when the turn carries tool calls.
    let tool_turn = CompletionResponse {
        output: vec![
            OutputItem::Text {
                text: "let me echo".into(),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
        ],
        usage: Usage {
            input_tokens: 100,
            output_tokens: 10,
            total_tokens: 110,
            non_cached_input_tokens: 100,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        },
        model: "stub-model".into(),
    };
    let final_turn = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "all done".into(),
        }],
        usage: Usage {
            input_tokens: 200,
            output_tokens: 20,
            total_tokens: 220,
            non_cached_input_tokens: 200,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        },
        model: "stub-model".into(),
    };
    let p = std::sync::Arc::new(FakeProvider::new(vec![tool_turn, final_turn]));
    let mut tools = ToolRegistry::new();
    tools.register(std::sync::Arc::new(StubTool::new("echo")));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let events = runner.store().replay(session).await.expect("replay");
    let count = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::TurnUsage { .. }))
        .count();
    assert_eq!(count, 2, "one TurnUsage per model call, not {count}");
}

#[tokio::test]
async fn test_recovery_retry_records_usage() {
    // A length-cut on call 1 (finish "length") fires the resume-direct
    // recovery loop: the partial reply + nudge append, then a re-call that
    // finishes "stop". Both calls burn real tokens, and the retry's input
    // grows (partial + nudge are appended before the re-call). Without
    // recording the retry path the worst offenders (long replies that retry
    // 1-3x) are the least visible in /cost + /trajectory — the exact blind
    // spot trajectory exists to surface. Pins: two TurnUsage events, the
    // retry flagged recovery=true and carrying the partial call's usage.
    let retry_usage = Usage {
        input_tokens: 1000,
        output_tokens: 500,
        total_tokens: 1500,
        non_cached_input_tokens: 200,
        cache_read_input_tokens: 800,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    let final_usage = Usage {
        input_tokens: 4000,
        output_tokens: 600,
        total_tokens: 4600,
        non_cached_input_tokens: 500,
        cache_read_input_tokens: 3500,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    let p = std::sync::Arc::new(ScriptRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t1".into() },
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "partial".into(),
            },
            LlmEvent::TextEnd { id: "t1".into() },
            LlmEvent::Finish {
                reason: "length".into(),
                usage: Some(retry_usage.clone()),
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t2".into() },
            LlmEvent::TextDelta {
                id: "t2".into(),
                text: " done".into(),
            },
            LlmEvent::TextEnd { id: "t2".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: Some(final_usage.clone()),
            },
        ],
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let events = runner.store().replay(session).await.expect("replay");
    let turn_usages: Vec<&houyicoder_context::TurnEvent> = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::TurnUsage { .. }))
        .collect();
    assert_eq!(
        turn_usages.len(),
        2,
        "retry + terminal each record a TurnUsage",
    );
    // First TurnUsage is the retry (same logical turn 1, call_in_turn 1).
    // Both calls share turn=1 — the fix for the turn_count semantic: a
    // length-recovery retry is the SAME turn, not a new turn.
    match &turn_usages[0].kind {
        TurnEventKind::TurnUsage {
            turn,
            call_in_turn,
            input_tokens,
            recovery,
            ..
        } => {
            assert_eq!(*turn, 1, "retry stays on turn 1, not a new turn");
            assert_eq!(*call_in_turn, 1, "retry is the first round-trip");
            assert_eq!(*input_tokens, 1000);
            assert!(*recovery, "the retry call must be flagged recovery");
        }
        _ => unreachable!(),
    }
    // Second is the terminal success (turn 1, call_in_turn 2, recovery false).
    match &turn_usages[1].kind {
        TurnEventKind::TurnUsage {
            turn,
            call_in_turn,
            input_tokens,
            recovery,
            ..
        } => {
            assert_eq!(*turn, 1, "terminal stays on turn 1 too");
            assert_eq!(*call_in_turn, 2, "terminal is the second round-trip");
            assert_eq!(*input_tokens, 4000);
            assert!(!*recovery, "the terminal call must not be flagged recovery");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn test_cancelled_records_no_usage() {
    // A stream cancelled mid-flight returns Ok(None) before the record_turn
    // sites (the retry + terminal arms both sit AFTER the stream loop), so no
    // TurnUsage lands and no cost/duration is recorded. Pins that cancel
    // leaves no half-finished timing state: api_start is a per-'outer local
    // dropped on return, and record_turn (the only thing that writes timing)
    // never fires on the cancel path — consistent with "cancelled calls do
    // not record usage".
    let p = std::sync::Arc::new(HangingProvider::new(vec![LlmEvent::StepStart { index: 0 }]));
    let runner = std::sync::Arc::new(runner_with(p, ToolRegistry::new()));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    runner.abort();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("run resolves on abort within 2s")
        .expect("run task")
        .expect("run ok");
    assert!(
        matches!(result.outcome, RunOutcome::Interrupted(_)),
        "expected Interrupted, got {:?}",
        result.outcome
    );
    let events = runner.store().replay(session).await.expect("replay");
    let turn_usages = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::TurnUsage { .. }))
        .count();
    assert_eq!(turn_usages, 0, "cancelled call must record no TurnUsage");
}
