//! Unit tests for the observability module.

use super::evolution::*;
use super::*;
use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_protocol::llm::Usage;
use std::path::PathBuf;

use crate::agent::HookError;

fn event(text: &str) -> TurnEvent {
    TurnEvent {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::UserInput {
            text: text.to_string(),
        },
    }
}

fn usage(input: u32, output: u32, cache_read: u32) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        non_cached_input_tokens: input - cache_read,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

#[test]
fn test_append_write_stores_event() {
    let mut log = ObservabilityLog::new(200_000);
    log.append_write(event("hello"));
    log.append_write(event("world"));
    assert_eq!(log.trajectory().len(), 2);
}

#[test]
fn test_tool_call_aggregates() {
    let mut log = ObservabilityLog::new(200_000);
    log.record_tool_call("bash", 100, false, None);
    log.record_tool_call("bash", 300, false, None);
    log.record_tool_call("bash", 50, true, Some("command not found"));
    let stats = log.tool_stats();
    assert_eq!(stats.len(), 1);
    let rec = &stats[0];
    assert_eq!(rec.tool_name, "bash");
    assert_eq!(rec.call_count, 3);
    assert_eq!(rec.fail_count, 1);
    assert!((rec.success_rate() - 2.0 / 3.0).abs() < 1e-9);
    assert_eq!(rec.total_duration_ms, 450);
    assert_eq!(rec.max_duration_ms, 300);
    assert!((rec.avg_duration_ms() - 150.0).abs() < 1e-9);
}

#[test]
fn test_tool_success_rate_empty() {
    let rec = ToolCallRecord::new("edit");
    assert_eq!(rec.call_count, 0);
    // Vacuous success: a fresh tool with no calls returns 1.0, not 0.0.
    assert!((rec.success_rate() - 1.0).abs() < 1e-9);
}

#[test]
fn test_failure_reason_error() {
    let o = serde_json::json!({"error": "permission denied"});
    assert_eq!(
        tool_failure_reason(&o).as_deref(),
        Some("permission denied")
    );
}

#[test]
fn test_failure_reason_stderr() {
    let o = serde_json::json!({"success": false, "exit_code": 1, "stderr": "error[E0308]: mismatched types\n  --> src/lib.rs:1:1"});
    assert_eq!(
        tool_failure_reason(&o).as_deref(),
        Some("error[E0308]: mismatched types")
    );
}

#[test]
fn test_failure_reason_stdout() {
    // A 2>&1 redirect merges stderr into stdout; the FAILED summary sits at
    // the end of stdout. The reason must come from stdout, not stderr.
    let o = serde_json::json!({"success": false, "exit_code": 101, "stdout": "running 1 test\ntest a ... FAILED\n\ntest result: FAILED. 1 failed"});
    let r = tool_failure_reason(&o);
    assert!(r.is_some(), "cargo test failure must yield a reason");
    assert!(r.as_deref().unwrap().to_lowercase().contains("failed"));
}

#[test]
fn test_failure_reason_grep_none() {
    // grep with no match exits 1 but emits no output — data, not a failure.
    // This is the case that must NOT inflate fail_count and trip the gate.
    let o = serde_json::json!({"success": false, "exit_code": 1, "stdout": "", "stderr": ""});
    assert!(tool_failure_reason(&o).is_none());
}

#[test]
fn test_failure_reason_success() {
    let o = serde_json::json!({"success": true, "exit_code": 0});
    assert!(tool_failure_reason(&o).is_none());
}

#[test]
fn test_tool_reasons_dedup_capped() {
    let mut log = ObservabilityLog::new(200_000);
    // Insert the same reason many times — dedup keeps one copy.
    for _ in 0..5 {
        log.record_tool_call("grep", 10, true, Some("no match"));
    }
    // Insert distinct reasons up to the cap.
    for i in 0..(FAILURE_REASONS_CAP as u32 + 5) {
        let reason = format!("error {i}");
        log.record_tool_call("grep", 10, true, Some(&reason));
    }
    let rec = &log.tool_stats()[0];
    assert!(rec.failure_reasons.len() <= FAILURE_REASONS_CAP);
    // The deduplicated reason should be present exactly once.
    let nm = rec
        .failure_reasons
        .iter()
        .filter(|r| r == &"no match")
        .count();
    assert_eq!(nm, 1);
}

#[test]
fn test_latency_p50_within_bounds() {
    let mut hist = LatencyHistogram::new();
    // Ten samples at 5 ms, ten at 500 ms.
    for _ in 0..10 {
        hist.record(5);
    }
    for _ in 0..10 {
        hist.record(500);
    }
    // p50 should land in the low bucket (upper bound 8).
    let p50 = hist.percentile(0.50);
    assert!(p50 <= 8, "p50 {p50} should be in low bucket");
    // p99 should land in the high bucket (upper bound 512).
    let p99 = hist.percentile(0.99);
    assert!(p99 >= 512, "p99 {p99} should be in high bucket");
    assert_eq!(hist.total_samples(), 20);
}

#[test]
fn test_latency_empty_returns_zero() {
    let hist = LatencyHistogram::new();
    assert_eq!(hist.percentile(0.50), 0);
    assert_eq!(hist.percentile(0.99), 0);
    assert_eq!(hist.total_samples(), 0);
}

#[test]
fn test_cost_accumulates_per_model() {
    let mut log = ObservabilityLog::new(200_000);
    let u1 = Usage {
        input_tokens: 1000,
        output_tokens: 500,
        total_tokens: 1500,
        non_cached_input_tokens: 200,
        cache_read_input_tokens: 800,
        cache_write_input_tokens: 100,
        reasoning_tokens: 50,
    };
    log.record_usage("model-a", &u1, 0, 120, 100, 200_000, 32_768);
    let u2 = Usage {
        input_tokens: 2000,
        output_tokens: 300,
        total_tokens: 2300,
        non_cached_input_tokens: 500,
        cache_read_input_tokens: 1500,
        cache_write_input_tokens: 200,
        reasoning_tokens: 30,
    };
    log.record_usage("model-a", &u2, 0, 80, 70, 200_000, 32_768);
    let cost = log.cost();
    assert_eq!(cost.by_model.len(), 1);
    let m = &cost.by_model["model-a"];
    assert_eq!(m.input_tokens, 3000);
    assert_eq!(m.output_tokens, 800);
    assert_eq!(m.cache_read_tokens, 2300);
    assert_eq!(m.cache_write_tokens, 300);
    assert_eq!(m.reasoning_tokens, 80);
    assert_eq!(cost.total_api_duration_ms, 200);
    assert_eq!(cost.total_api_duration_without_retries_ms, 170);
}

#[test]
fn test_turn_delta_ratios() {
    let mut log = ObservabilityLog::new(200_000);
    // context_pct uses the provider's measured input_tokens (1000) as the
    // source of truth — NOT the 30k local served fallback (ignored when
    // input_tokens > 0), and NOT cumulative.input (per-turn, so it does not
    // false-ceiling as turns accumulate).
    let u = Usage {
        input_tokens: 1000,
        output_tokens: 500,
        total_tokens: 1500,
        non_cached_input_tokens: 200,
        cache_read_input_tokens: 800,
        cache_write_input_tokens: 100,
        reasoning_tokens: 50,
    };
    log.start_turn();
    log.record_usage("model-a", &u, 30_000, 50, 50, 200_000, 32_768);
    let delta = log.last_turn_delta().expect("delta recorded");
    assert_eq!(delta.turn, 1);
    // call_in_turn is 1 (first + only call this turn). Retries would advance
    // it without advancing turn — pinned end-to-end in turn_usage_emit_tests.
    assert_eq!(delta.call_in_turn, 1);
    assert_eq!(delta.input, 1000);
    assert_eq!(delta.cache_read, 800);
    // cache_hit_ratio is Some(800/1000) = 0.8 (input > 0). None only when
    // input is zero (usage omitted) — never 0.0 for an unknown rate.
    let ratio = delta.cache_hit_ratio.expect("cache_hit_ratio known");
    assert!((ratio - 800.0 / 1000.0).abs() < 1e-9);
    // context_pct = per-turn input_tokens / window = 1000 / 200000, NOT the
    // 30k fallback (input_tokens > 0 wins) and NOT cumulative (per-turn).
    let pct = delta.context_pct.expect("context_pct known");
    assert!((pct - 1000.0 / 200_000.0).abs() < 1e-9);
    assert_eq!(delta.cumulative.input_tokens, 1000);
}

#[test]
fn test_context_pct_fallback() {
    // A streaming proxy that omits usage (input_tokens 0): context_pct falls
    // back to the local served count, never a silent 0%. Same dual-number
    // convention as TruncationVerdict's server/self output-token pair.
    let mut log = ObservabilityLog::new(200_000);
    let u = usage(0, 500, 0);
    log.record_usage("model-a", &u, 30_000, 50, 50, 200_000, 32_768);
    let delta = log.last_turn_delta().expect("delta recorded");
    let pct = delta.context_pct.expect("fallback serves the local count");
    assert!((pct - 30_000.0 / 200_000.0).abs() < 1e-9);
}

#[test]
fn test_context_pct_unknown() {
    // Provider usage zero AND served count zero: the fill is unknown, not
    // 0%. None so a consumer renders "—" rather than a plausible-but-wrong
    // 0% (the bug class this log exists to kill — #73/#75).
    let mut log = ObservabilityLog::new(200_000);
    let u = usage(0, 0, 0);
    log.record_usage("model-a", &u, 0, 50, 50, 200_000, 32_768);
    let delta = log.last_turn_delta().expect("delta recorded");
    assert!(
        delta.context_pct.is_none(),
        "unknown fill must be None not 0.0"
    );
}

#[test]
fn test_context_view_projects() {
    let mut log = ObservabilityLog::new(200_000);
    log.append_write(event("first"));
    log.record_tool_call("bash", 50, false, None);
    let view: &dyn MetricsView = &log;
    assert_eq!(view.trajectory().len(), 1);
    assert_eq!(view.tool_stats().len(), 1);
    assert!(view.report().is_none());
    let _cost = view.cost();
    assert!(view.turn_delta().is_none());
}

#[test]
fn test_log_reset_clears_state() {
    let mut log = ObservabilityLog::new(200_000);
    log.append_write(event("data"));
    log.record_tool_call("bash", 50, false, None);
    log.record_usage("model-a", &usage(10, 5, 0), 0, 5, 5, 200_000, 32_768);
    log.record_hook_error(HookError::Timeout {
        hook_name: "h".into(),
        limit_ms: 5000,
    });
    assert!(!log.trajectory().is_empty());
    assert!(!log.tool_stats().is_empty());
    assert!(log.last_turn_delta().is_some());
    log.reset();
    assert!(log.trajectory().is_empty());
    assert!(log.tool_stats().is_empty());
    assert!(log.last_turn_delta().is_none());
    assert_eq!(log.cost().by_model.len(), 0);
}

#[test]
fn test_failure_record_caps() {
    let mut rec = FailureRecord {
        failure_id: FailureId {
            tool_name: "bash".into(),
            call_id: "call_1".into(),
            turn: 1,
        },
        tool_name: "bash".into(),
        attempt_count: 10,
        attempt_call_ids: (0..10).map(|i| format!("call_{i}")).collect(),
        failure_reasons: (0..20).map(|i| (i, format!("reason {i}"))).collect(),
        state_ref: TrajectoryRef {
            turn_id: 1,
            event_range: (EventId::new(), EventId::new()),
        },
        error_type: ErrorCategory::ToolError,
        error_message: "x".repeat(1000),
        affected_paths: (0..20).map(|i| PathBuf::from(format!("/p/{i}"))).collect(),
        fix_chain: (0..20)
            .map(|i| FixLink {
                edit: TrajectoryRef {
                    turn_id: i,
                    event_range: (EventId::new(), EventId::new()),
                },
                error_addressed: format!("err {i}"),
                turn: i,
                confidence: FixConfidence::Suspected,
                evidence: Vec::new(),
            })
            .collect(),
        outcome: FailureOutcome::Abandoned,
        recovery_turn: None,
    };
    rec.apply_all_caps();
    assert!(rec.failure_reasons.len() <= FAILURE_REASONS_PER_EPISODE_CAP);
    assert!(rec.fix_chain.len() <= FIX_CHAIN_CAP);
    assert!(rec.error_message.chars().count() < 1000);
    assert!(rec.affected_paths.len() <= AFFECTED_PATHS_CAP);
}

#[test]
fn test_truncate_marks_overflow() {
    let s = truncate_str("hello world", 5);
    assert!(s.starts_with("hello"));
    assert!(s.contains("(+"));
    let s2 = truncate_str("hi", 10);
    assert_eq!(s2, "hi");
}

#[test]
fn test_two_models_tracked_separately() {
    let mut log = ObservabilityLog::new(200_000);
    log.record_usage("model-a", &usage(100, 50, 80), 0, 30, 30, 200_000, 32_768);
    log.record_usage("model-b", &usage(200, 30, 120), 0, 40, 40, 200_000, 32_768);
    let cost = log.cost();
    assert_eq!(cost.by_model.len(), 2);
    assert_eq!(cost.by_model["model-a"].input_tokens, 100);
    assert_eq!(cost.by_model["model-b"].input_tokens, 200);
    assert_eq!(cost.total_api_duration_ms, 70);
}
