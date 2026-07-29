//! Observability wiring helpers: keep the agent loop's call sites thin.
//!
//! The runner holds the observability log behind a shared mutex (same shape
//! as the usage accumulator). These free functions take the shared handle,
//! lock briefly, and delegate to the log's own methods — so the drive loop
//! and resolve_turn stay one-liners and the lock wrappers do not bloat the
//! runner module past its file-size budget.

use std::sync::{Arc, Mutex};

use houyicoder_protocol::llm::Usage;

use crate::observability::ObservabilityLog;

/// Shared observability log handle.
pub type SharedObservability = Arc<Mutex<ObservabilityLog>>;

/// Build a shared log with NoCharge pricing. Real pricing wires when cost
/// USD is needed; the loop records usage + tool stats regardless.
pub fn new_log(context_window: u32) -> SharedObservability {
    Arc::new(Mutex::new(ObservabilityLog::new(context_window)))
}

/// Mark the start of a logical turn at the model_call_stream entry. Increments
/// the logical turn counter + resets the per-turn round-trip index, so
/// length/overflow retries inside one entry stay on the same turn — the
/// turn boundary is the model-call entry.
pub fn start_turn(obs: &SharedObservability) {
    if let Ok(mut ol) = obs.lock() {
        ol.start_turn();
    }
}

/// Record a turn's usage + cost (G3 live meter). served_token_count is the
/// local tiktoken count of what was sent this turn, the fallback when the
/// provider omits usage (so context_pct is never a silent 0%). api_duration_ms
/// is the wall-clock length of this round-trip (stream start → Finish);
/// api_duration_without_retries is NOT taken here (passed 0) — it refers to
/// transport-layer retries (network re-attempts within one call), NOT to
/// length-recovery (a separate logical round-trip recorded on its own). Do
/// not fold the two when the without_retries tracking wires.
pub fn record_turn(
    obs: &SharedObservability,
    model: &str,
    usage: &Usage,
    served_token_count: u32,
    api_duration_ms: u64,
    context_window: u32,
    max_output_tokens: u32,
) {
    if let Ok(mut ol) = obs.lock() {
        ol.record_usage(
            model,
            usage,
            served_token_count,
            api_duration_ms,
            0,
            context_window,
            max_output_tokens,
        );
    }
}

/// Record per-tool stats (count/fail/reasons) from a turn's result list.
/// call_names maps result call_id to tool name. Delegates to the log's own
/// method; duration_ms is 0 until per-call timing wires in the executor.
pub fn record_tool_outcomes(
    obs: &SharedObservability,
    results: &[(String, serde_json::Value)],
    call_names: &std::collections::HashMap<String, String>,
) {
    if let Ok(mut ol) = obs.lock() {
        ol.record_tool_outcomes(results, call_names);
    }
}

#[cfg(test)]
mod tests {
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
        // served_token_count is the fallback; with input_tokens > 0 the
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
    fn test_record_turn_served_fallback() {
        // A streaming proxy that omits usage: input_tokens 0, so context_pct
        // falls back to the local served_token_count. Never a silent 0%.
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
        // Both provider usage and served count zero: the fill is unknown, not
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
        let mut names = std::collections::HashMap::new();
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
}
