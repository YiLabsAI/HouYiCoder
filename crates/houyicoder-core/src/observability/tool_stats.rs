//! Per-tool aggregate statistics and the single failure predicate.
//!
//! Records aggregate by tool name rather than per call, so memory is bounded
//! by the number of tools; the failure reasons are deduplicated and capped for
//! the same reason.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::latency::LatencyHistogram;

/// Maximum distinct failure reasons retained per tool before truncation.
pub(crate) const FAILURE_REASONS_CAP: usize = 20;
/// Maximum chars of an individual failure reason string.
pub(crate) const REASON_CAP: usize = 200;

/// Per-tool aggregate statistics. Aggregated by tool name (not per call) to
/// bound memory. Failure reasons are deduplicated and capped. Latency is
/// tracked as totals plus a bucketed histogram for tail-latency estimation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub call_count: u32,
    pub fail_count: u32,
    pub failure_reasons: Vec<String>,
    pub total_duration_ms: u64,
    pub last_duration_ms: u64,
    pub max_duration_ms: u64,
    pub latency_buckets: LatencyHistogram,
}

impl ToolCallRecord {
    pub fn new(name: &str) -> Self {
        Self {
            tool_name: name.to_string(),
            call_count: 0,
            fail_count: 0,
            failure_reasons: Vec::new(),
            total_duration_ms: 0,
            last_duration_ms: 0,
            max_duration_ms: 0,
            latency_buckets: LatencyHistogram::new(),
        }
    }

    /// Success rate as a fraction in [0.0, 1.0]. Returns 1.0 when no calls
    /// have been recorded (vacuous success, not zero, to avoid alarming
    /// the user on a fresh log).
    pub fn success_rate(&self) -> f64 {
        if self.call_count == 0 {
            1.0
        } else {
            (self.call_count - self.fail_count) as f64 / self.call_count as f64
        }
    }

    /// Mean duration across all calls. Returns 0.0 when empty.
    pub fn avg_duration_ms(&self) -> f64 {
        if self.call_count == 0 {
            0.0
        } else {
            self.total_duration_ms as f64 / self.call_count as f64
        }
    }

    /// Estimated p50 (median) latency in ms.
    pub fn p50_ms(&self) -> u64 {
        self.latency_buckets.percentile(0.50)
    }

    /// Estimated p99 (tail) latency in ms.
    pub fn p99_ms(&self) -> u64 {
        self.latency_buckets.percentile(0.99)
    }
}

/// Whether a tool result is a failure, and the reason if so.
///
/// Failure = an "error" field (execute-level failure) OR "success": false
/// with a diagnosable failure line (a bash non-zero exit). An exit code
/// with no diagnosable output — grep with no match, test -f on a missing
/// path — is data, not a failure: it does not inflate the fail_count that
/// feeds the reward-dream gate, so a benign non-zero exit cannot trip it.
/// The reason is stderr's first line when stderr is non-empty (the
/// compiler error root cause), else the last matching line of stdout
/// (cargo prints "test result: FAILED" at the end). This is the single
/// failure predicate for the agent loop so the observability tally, the
/// hook pipeline, the redundancy retry test, and the TUI status count
/// stay in sync.
pub fn tool_failure_reason(output: &Value) -> Option<Cow<'_, str>> {
    if let Some(err) = output.get("error").and_then(|v| v.as_str()) {
        return Some(Cow::Borrowed(err));
    }
    if output.get("success").and_then(|v| v.as_bool()) != Some(false) {
        return None;
    }
    let stderr = output.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    if !stderr.is_empty() {
        return stderr.lines().next().map(Cow::Borrowed);
    }
    let stdout = output.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    stdout
        .lines()
        .rev()
        .find(|line| {
            let l = line.to_ascii_lowercase();
            l.contains("failed")
                || l.contains("error")
                || l.contains("panic")
                || l.contains("assertion")
        })
        .map(|line| Cow::Owned(line.to_string()))
}
