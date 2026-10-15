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
use std::collections::HashMap;

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

/// Record a turn's usage + cost (G3 live meter). estimated_input_tokens is
/// the local tiktoken count of what was sent, the fallback when the
/// provider omits usage (context_pct never reads a silent 0%).
/// api_duration_ms is the stream start → Finish wall-clock. The final 0 is
/// the transport-retry duration slot: length-recovery is a separate
/// logical round-trip recorded on its own, not folded into it.
pub fn record_turn(
    obs: &SharedObservability,
    model: &str,
    usage: &Usage,
    estimated_input_tokens: u32,
    api_duration_ms: u64,
    context_window: u32,
    max_output_tokens: u32,
) {
    if let Ok(mut ol) = obs.lock() {
        ol.record_usage(
            model,
            usage,
            estimated_input_tokens,
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
    call_names: &HashMap<String, String>,
) {
    if let Ok(mut ol) = obs.lock() {
        ol.record_tool_outcomes(results, call_names);
    }
}

#[cfg(test)]
mod tests;
