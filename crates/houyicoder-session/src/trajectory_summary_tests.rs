//! Tests for the session-wide trajectory summary: what each event folds in,
//! what the turn bookkeeping reports, and what a read costs.

use super::*;
use houyicoder_context::{EventId, SessionId};

fn entry(ts: u64, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts,
        prev_hash: None,
        event,
    }
}

fn usage(ts: u64, input: u64, output: u64, model: &str) -> SessionLogEntry {
    entry(
        ts,
        SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: input / 2,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: model.into(),
            recovery: false,
            effort: None,
        },
    )
}

fn timing(ts: u64, total_ms: u64, ttft_ms: u64, decode_ms: u64) -> SessionLogEntry {
    entry(
        ts,
        SessionEvent::ModelStepTiming {
            turn: 1,
            step: 1,
            total_ms,
            ttft_ms: Some(ttft_ms),
            decode_ms: Some(decode_ms),
        },
    )
}

fn fold(events: &[SessionLogEntry]) -> TrajectorySummary {
    let mut state = TrajectorySummaryState::default();
    for event in events {
        state.record(event);
    }
    state.snapshot()
}

#[test]
fn test_empty_is_unknown() {
    let summary = fold(&[]);
    assert_eq!(summary.total_turns, 0);
    assert!(!summary.usage.totals_known, "no turn reported anything");
    assert_eq!(summary.timing.ttft_samples, 0);
    assert_eq!(summary.models_used, 0);
}

#[test]
fn test_totals_fold() {
    let events = vec![
        entry(0, SessionEvent::UserInput { text: "a".into() }),
        usage(10, 100, 20, "m"),
        usage(20, 50, 5, "m"),
        entry(30, SessionEvent::RunCompleted { secs: Some(1) }),
    ];
    let summary = fold(&events);
    assert_eq!(summary.total_turns, 1);
    assert_eq!(summary.usage.input_tokens, 150);
    assert_eq!(summary.usage.output_tokens, 25);
    assert_eq!(summary.usage.cache_read_tokens, 75);
    assert!(summary.usage.totals_known, "the turn reported usage");
    assert_eq!(summary.models_used, 1);
    assert_eq!(summary.single_model.as_deref(), Some("m"));
    assert_eq!(summary.duration_ms, 30);
}

/// A turn that reaches its end without usage leaves the session total a lower
/// bound. Two model calls in the first turn must not stand in for the second
/// turn's missing usage, which is what a count comparison would do.
#[test]
fn test_missing_usage_unknown() {
    let events = vec![
        entry(0, SessionEvent::UserInput { text: "a".into() }),
        usage(10, 100, 20, "m"),
        usage(11, 100, 20, "m"),
        entry(20, SessionEvent::UserInput { text: "b".into() }),
        entry(30, SessionEvent::RunCompleted { secs: Some(1) }),
    ];
    let summary = fold(&events);
    assert_eq!(summary.total_turns, 2);
    assert!(
        !summary.usage.totals_known,
        "the second turn never reported usage"
    );
}

/// An open turn has not reported usage yet, so the session total is a lower
/// bound while it runs.
#[test]
fn test_open_turn_is_unknown() {
    let events = vec![entry(0, SessionEvent::UserInput { text: "a".into() })];
    assert!(!fold(&events).usage.totals_known);
}

/// A log that opens mid-run carries a turn the fold numbers, so the pane's
/// count and numbering do not start below what the session really ran.
#[test]
fn test_leading_partial_turn_counts() {
    let events = vec![
        usage(0, 10, 1, "m"),
        entry(10, SessionEvent::UserInput { text: "a".into() }),
        usage(20, 10, 1, "m"),
    ];
    let summary = fold(&events);
    assert_eq!(
        summary.total_turns, 2,
        "one numbered turn before the first user input, plus one after"
    );
}

/// A log that opens mid-run numbers its first stretch as a turn, so a turn
/// that reported no usage must leave the total unknown rather than looking
/// like a session that spent nothing.
#[test]
fn test_leading_partial_turn_unknown() {
    let events = vec![entry(
        0,
        SessionEvent::SubagentReturn {
            child_session_id: "child".into(),
            status: "completed".into(),
            summary: String::new(),
            result_ref: "child".into(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        },
    )];
    let summary = fold(&events);
    assert_eq!(summary.total_turns, 1, "the fold numbers it");
    assert!(
        !summary.usage.totals_known,
        "and it never reported usage for that turn"
    );
}

/// Distinct models are a session fact, not a page fact.
#[test]
fn test_models_span_the_session() {
    let events = vec![
        entry(0, SessionEvent::UserInput { text: "a".into() }),
        usage(10, 10, 1, "qwen"),
        entry(20, SessionEvent::UserInput { text: "b".into() }),
        usage(30, 10, 1, "deepseek"),
    ];
    let summary = fold(&events);
    assert_eq!(summary.models_used, 2);
    assert!(
        summary.single_model.is_none(),
        "two models name no single one"
    );
}

#[test]
fn test_timing_folds() {
    let events = vec![timing(0, 1000, 200, 800), timing(10, 2000, 400, 1600)];
    let summary = fold(&events);
    assert_eq!(summary.timing.ttft_samples, 2);
    assert_eq!(
        summary.timing.ttft_avg_ms,
        Some(300),
        "the average is exact"
    );
    assert_eq!(summary.timing.model_ms, 3000);
    assert_eq!(summary.timing.decode_samples, 2);
}

/// A percentile reports the bucket that holds the rank, so it is an upper
/// bound rather than the exact sample.
#[test]
fn test_percentile_bucket_bounds() {
    let events: Vec<SessionLogEntry> = (0..100).map(|i| timing(i, 10, i * 10, 1)).collect();
    let summary = fold(&events);
    let p95 = summary.timing.ttft_p95_ms.expect("samples");
    // The 95th of 0, 10, ... 990 is 940, which sits in the 50 ms bucket
    // covering 900 to 950.
    assert_eq!(p95, 950, "the upper bound of the bucket holding the rank");
    assert!(!summary.timing.ttft_percentile_capped);
}

/// A latency beyond the wide range lands in the overflow bucket, and the read
/// says so instead of presenting the bound as the value.
#[test]
fn test_percentile_overflow_capped() {
    let events = vec![timing(0, 10, 900_000, 1)];
    let summary = fold(&events);
    assert_eq!(summary.timing.ttft_p95_ms, Some(600_000));
    assert!(summary.timing.ttft_percentile_capped);
}

#[test]
fn test_decode_speed() {
    let events = vec![
        entry(0, SessionEvent::UserInput { text: "a".into() }),
        usage(10, 100, 500, "m"),
        timing(11, 1000, 100, 1000),
    ];
    let summary = fold(&events);
    let tps = summary.timing.decode_tok_per_sec.expect("decode span");
    assert!((tps - 500.0).abs() < 0.01, "500 tokens over 1s: {tps}");
}

fn result(call_id: &str, output: serde_json::Value, duration_ms: u64) -> SessionLogEntry {
    entry(
        0,
        SessionEvent::ToolResult {
            call_id: call_id.into(),
            output,
            duration_ms,
        },
    )
}

#[test]
fn test_semantic_exit_passes() {
    let events = vec![
        entry(
            0,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "grep foo bar"}),
            },
        ),
        result(
            "c1",
            serde_json::json!({"success": false, "exit_code": 1}),
            5,
        ),
    ];
    let summary = fold(&events);
    assert_eq!(summary.usage.failures, 0, "grep finding no match succeeded");
    assert_eq!(summary.timing.tool_ms, 5, "and its duration still counts");
}

#[test]
fn test_plain_failure_counts() {
    let events = vec![
        entry(
            0,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "false"}),
            },
        ),
        result(
            "c1",
            serde_json::json!({"success": false, "exit_code": 1}),
            5,
        ),
    ];
    assert_eq!(fold(&events).usage.failures, 1);
}

/// A result whose call the fold never saw is judged on the output alone, the
/// same rule the trajectory projection applies to a windowed read.
#[test]
fn test_orphan_result_plain_rule() {
    let events = vec![
        result("c1", serde_json::json!({"success": false}), 0),
        result("c2", serde_json::json!({"error": "boom"}), 0),
    ];
    assert_eq!(fold(&events).usage.failures, 2);
}

/// A turn's calls are retired when the turn ends, so an interrupted turn's
/// calls do not accumulate for the rest of the session.
#[test]
fn test_pending_calls_retired() {
    let mut state = TrajectorySummaryState::default();
    state.record(&entry(0, SessionEvent::UserInput { text: "a".into() }));
    for i in 0..50 {
        state.record(&entry(
            1,
            SessionEvent::ToolCall {
                call_id: format!("c{i}"),
                tool: "bash".into(),
                input: serde_json::json!({"command": "x"}),
            },
        ));
    }
    assert_eq!(state.pending_calls.len(), 50);
    state.record(&entry(2, SessionEvent::UserInput { text: "b".into() }));
    assert_eq!(
        state.pending_calls.len(),
        0,
        "the previous turn's calls are gone"
    );
}

#[test]
fn test_delegated_usage_folds() {
    let events = vec![entry(
        0,
        SessionEvent::SubagentReturn {
            child_session_id: "child".into(),
            status: "completed".into(),
            summary: String::new(),
            result_ref: "child".into(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: 80,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        },
    )];
    let summary = fold(&events);
    assert_eq!(summary.usage.subagent.calls, 1);
    assert_eq!(summary.usage.subagent.input_tokens, 100);
    assert!(!summary.usage.subagent_unmeasured);
}

#[test]
fn test_unmeasured_child_flagged() {
    let events = vec![entry(
        0,
        SessionEvent::SubagentReturn {
            child_session_id: "child".into(),
            status: "completed".into(),
            summary: String::new(),
            result_ref: "child".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        },
    )];
    assert!(fold(&events).usage.subagent_unmeasured);
}

/// A streaming delta advances the span but no counter: it is not durable, so
/// counting it would make a live session differ from the same session read
/// back.
#[test]
fn test_delta_moves_span_only() {
    let mut state = TrajectorySummaryState::default();
    state.record(&entry(0, SessionEvent::UserInput { text: "a".into() }));
    state.record(&usage(10, 100, 20, "m"));
    let before = state.snapshot();
    state.record(&entry(
        30,
        SessionEvent::AssistantTextDelta { text: "hi".into() },
    ));
    let after = state.snapshot();
    assert_eq!(after.duration_ms, 30, "the span reaches the delta");
    assert_eq!(after.usage.input_tokens, before.usage.input_tokens);
    assert_eq!(after.timing.model_ms, before.timing.model_ms);
    assert_eq!(after.total_turns, before.total_turns);
}
