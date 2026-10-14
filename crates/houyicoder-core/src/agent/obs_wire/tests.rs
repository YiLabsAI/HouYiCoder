//! Tests for this module.

use super::*;
use crate::observability::MetricsView;
use houyicoder_protocol::llm::Usage;

fn usage(input: u32, cache_read: u32) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: 500,
        total_tokens: input + 500,
        non_cached_input_tokens: input - cache_read,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

#[test]
fn test_record_turn_captures_usage() {
    let obs = new_log(200_000);
    // estimated_input_tokens is the fallback; with input_tokens > 0 the
    // provider's count is primary, so context_pct = 1000 / 200_000.
    // api_duration_ms=500 flows through to the cost tally.
    record_turn(
        &obs,
        "model-a",
        &usage(1000, 800),
        30_000,
        500,
        200_000,
        32_768,
    );
    let ol = obs.lock().unwrap();
    let delta = ol.last_turn_delta().expect("delta recorded");
    assert_eq!(delta.input, 1000);
    assert_eq!(delta.cache_read, 800);
    // cache_read / input (input inclusive of cache_read) = 0.8, not 0.44.
    // Some(0.8) — known because input > 0; None only when input is 0.
    let ratio = delta.cache_hit_ratio.expect("cache_hit_ratio known");
    assert!((ratio - 0.8).abs() < 1e-9);
    // provider input_tokens primary: 1000 / 200_000, not the 30k fallback.
    let pct = delta.context_pct.expect("context_pct known");
    assert!((pct - 1000.0 / 200_000.0).abs() < 1e-9);
    // The round-trip duration lands in the cost tally (total, not
    // without_retries — that split is deferred).
    assert_eq!(ol.cost().total_api_duration_ms, 500);
}

#[test]
fn test_record_turn_estimate_fallback() {
    // A streaming proxy that omits usage: input_tokens 0, so context_pct
    // falls back to the local estimate. Never a silent 0%.
    let obs = new_log(200_000);
    let zero_usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        non_cached_input_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    record_turn(&obs, "model-a", &zero_usage, 30_000, 0, 200_000, 32_768);
    let ol = obs.lock().unwrap();
    let delta = ol.last_turn_delta().expect("delta recorded");
    let pct = delta.context_pct.expect("fallback serves the local count");
    assert!((pct - 30_000.0 / 200_000.0).abs() < 1e-9);
}

#[test]
fn test_record_turn_unknown_fill() {
    // Both provider usage and estimate zero: the fill is unknown, not
    // 0% — None so a future consumer can render "—" instead of a wrong 0%.
    let obs = new_log(200_000);
    let zero_usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        non_cached_input_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    record_turn(&obs, "model-a", &zero_usage, 0, 0, 200_000, 32_768);
    let ol = obs.lock().unwrap();
    let delta = ol.last_turn_delta().expect("delta recorded");
    assert!(
        delta.context_pct.is_none(),
        "unknown fill must be None not 0.0"
    );
}

#[test]
fn test_record_tool_outcomes_aggregates() {
    let obs = new_log(200_000);
    let mut names = HashMap::new();
    names.insert("c1".to_string(), "bash".to_string());
    names.insert("c2".to_string(), "bash".to_string());
    let results = vec![
        ("c1".to_string(), serde_json::json!({"ok": true})),
        ("c2".to_string(), serde_json::json!({"error": "boom"})),
    ];
    record_tool_outcomes(&obs, &results, &names);
    let ol = obs.lock().unwrap();
    let stats = ol.tool_stats();
    let bash = stats
        .iter()
        .find(|r| r.tool_name == "bash")
        .expect("bash recorded");
    assert_eq!(bash.call_count, 2);
    assert_eq!(bash.fail_count, 1);
}
