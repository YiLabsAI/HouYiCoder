//! Observability: cross-cutting collection and projection layer.
//!
//! The log is an append-only container that gathers records from every
//! subsystem (context lifecycle, tool pipeline, hook dispatch, provider
//! responses). It does not mutate any subsystem state — it only appends and
//! aggregates. The ContextView trait projects the log contents as read-only
//! snapshots for a host to render inspection commands.
//!
//! Token semantics follow the protocol layer Usage struct: inclusive totals
//! (input includes cached, output includes reasoning) plus a non-overlapping
//! breakdown. Cost aggregation tracks per-model usage and USD, aligned
//! field-for-field with the ModelUsage shape (input, output, cache read,
//! output, cache read, cache write, web search, cost, context window, max
//! output). The live per-turn delta goes beyond a last-response snapshot by
//! deriving cache hit ratio and context-fill percentage per turn.
//!
//! Self-evolution records (experience, lesson, skill, reflection, failure,
//! cross-run link) are defined as types in the evolution submodule; storage
//! and wiring are not built yet. The failure record carries a zero-copy
//! pointer into the raw trajectory log rather than duplicating state, mirroring
//! the context layer block-ref pattern: anchor plus reference, not copy.
//! Multi-level caps on every bounded field prevent unbounded growth from
//! pathological failure storms.

pub mod evolution;
#[cfg(test)]
#[path = "observability_tests.rs"]
mod tests;

use std::collections::HashMap;
use std::time::Instant;

use houyicoder_context::TurnEvent;
use houyicoder_protocol::llm::Usage;
use serde::{Deserialize, Serialize};

use crate::agent::{CompressResult, HookError};

// ===== bounds =====

/// Maximum distinct failure reasons retained per tool before truncation.
pub(crate) const FAILURE_REASONS_CAP: usize = 20;
/// Maximum chars of an individual failure reason string.
pub(crate) const REASON_CAP: usize = 200;

// ===== latency histogram =====

/// Bucketed latency histogram for p50/p99 estimation without retaining
/// every sample. Sixteen log2-spaced buckets span 1 ms to ~33 s; the final
/// bucket catches everything beyond. Recording is O(1); percentile lookup
/// walks at most 16 buckets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyHistogram {
    buckets: [u32; 16],
    total_samples: u32,
}

/// Upper-bound duration (ms) for each of the 16 buckets. Bucket i holds
/// samples in (BOUNDS[i-1], BOUNDS[i]]; bucket 0 holds [0, 1]. The last
/// bucket holds everything above BOUNDS[14].
const HISTOGRAM_BOUNDS_MS: [u64; 16] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
];

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            buckets: [0; 16],
            total_samples: 0,
        }
    }

    /// Increment the bucket that covers the given duration. O(1).
    pub fn record(&mut self, duration_ms: u64) {
        let idx = HISTOGRAM_BOUNDS_MS
            .iter()
            .position(|&b| duration_ms <= b)
            .unwrap_or(HISTOGRAM_BOUNDS_MS.len());
        let idx = idx.min(self.buckets.len() - 1);
        self.buckets[idx] += 1;
        self.total_samples += 1;
    }

    /// Estimate the p-th percentile (0.0..=1.0) duration in ms. Walks the
    /// buckets accumulating counts until the target percentile is reached,
    /// then returns the bucket upper bound. Returns 0 when empty.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.total_samples == 0 {
            return 0;
        }
        let target = (self.total_samples as f64 * p).ceil() as u32;
        let mut running = 0u32;
        for (i, &count) in self.buckets.iter().enumerate() {
            running += count;
            if running >= target {
                return HISTOGRAM_BOUNDS_MS[i.min(HISTOGRAM_BOUNDS_MS.len() - 1)];
            }
        }
        HISTOGRAM_BOUNDS_MS[HISTOGRAM_BOUNDS_MS.len() - 1]
    }

    pub fn total_samples(&self) -> u32 {
        self.total_samples
    }
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

// ===== tool call record =====

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

// ===== token and cost types =====

/// Live per-turn token delta. Captures the incremental tokens this turn
/// consumed plus the cumulative tally and derived ratios for a live status
/// display. Goes beyond a last-response snapshot by computing cache hit
/// ratio and context-fill percentage each turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTokenDelta {
    pub turn: u32,
    /// Round-trip index within this logical turn (1-based; a length-recovery
    /// retry is the same turn, so a 2-retry turn has call_in_turn 1, 2, 3).
    /// Pairs with the durable TurnUsage recovery flag so /trajectory renders
    /// "the turn with its retry count" (count the recovery=true calls in the turn) instead of
    /// exploding one user instruction into separate turns.
    pub call_in_turn: u32,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Cumulative usage across all turns this session.
    pub cumulative: Usage,
    /// cache_read_input_tokens / input_tokens. input_tokens is inclusive of
    /// cache_read (see the Usage struct), so cache_read is not added to the
    /// denominator — that would double-count and roughly halve the ratio.
    /// None when input is zero (a streaming proxy that omits usage, or a
    /// degenerate empty input) so an unknown hit rate is never shown as 0% —
    /// the same unknown-must-be-None rule as context_pct, since the two sit
    /// side by side on the status bar. Some(0.0) means "confirmed zero cache
    /// hit" (real input, no cache), distinct from None. Aligns the OTel
    /// convention: cache_read.input_tokens SHOULD be in input_tokens.
    pub cache_hit_ratio: Option<f64>,
    /// Current served-view occupancy: the provider's measured input_tokens
    /// (what it counted as served) over context_window, clamped to [0.0, 1.0].
    /// Falls back to the local tiktoken served count when the provider omits
    /// usage (common for streaming proxies — the same dual-number convention
    /// as TruncationVerdict's server_output_tokens / self_count_output_tokens).
    /// None when both are zero, so an unknown fill is never displayed as 0% —
    /// a plausible-but-wrong number is the bug class this log exists to kill.
    /// NOT cumulative.input, which grows unbounded and would false-ceiling.
    /// The status bar computes the same per-turn input_tokens / window from
    /// the live StatusSnapshot; the two are separate fields with one source,
    /// so they must not diverge — see status.rs last_input_tokens.
    pub context_pct: Option<f64>,
}

/// Per-model usage breakdown. Fields track the ModelUsage shape so cost
/// reports are directly comparable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub web_search_requests: u64,
    pub cost_usd: f64,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

/// Aggregated cost summary for a session. Carries the total USD, per-model
/// breakdown, API and wall durations, and code-change counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    pub total_cost_usd: f64,
    pub by_model: HashMap<String, ModelUsage>,
    pub total_api_duration_ms: u64,
    pub total_wall_duration_ms: u64,
    pub total_api_duration_without_retries_ms: u64,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Per-model pricing rates (USD per call). A real table is wired in a later
/// phase; the trait lets the log compute cost without hard-coding rates.
pub trait PricingTable: Send + Sync {
    /// Compute the USD cost for one model call's usage. Returns 0.0 for
    /// unknown models; the host can surface a warning separately.
    fn cost_for(&self, model: &str, usage: &Usage) -> f64;
}

/// A pass-through table that charges nothing. Used in tests and as a
/// placeholder until a real pricing table is wired.
pub struct NoCharge;

impl PricingTable for NoCharge {
    fn cost_for(&self, _model: &str, _usage: &Usage) -> f64 {
        0.0
    }
}

// ===== cost accumulator =====

/// Internal accumulator that folds per-model usage and cost across turns.
/// Not serialized directly; its snapshot is CostSummary.
#[derive(Debug, Clone)]
pub struct CostAccumulator {
    by_model: HashMap<String, ModelUsage>,
    total_cost_usd: f64,
    total_api_duration_ms: u64,
    total_api_duration_without_retries_ms: u64,
    lines_added: u32,
    lines_removed: u32,
}

impl Default for CostAccumulator {
    fn default() -> Self {
        Self {
            by_model: HashMap::new(),
            total_cost_usd: 0.0,
            total_api_duration_ms: 0,
            total_api_duration_without_retries_ms: 0,
            lines_added: 0,
            lines_removed: 0,
        }
    }
}

impl CostAccumulator {
    /// Fold one response's usage and cost into the per-model tally.
    pub fn record(
        &mut self,
        model: &str,
        usage: &Usage,
        cost: f64,
        api_duration_ms: u64,
        api_duration_without_retries_ms: u64,
    ) {
        let entry = self.by_model.entry(model.to_string()).or_default();
        entry.input_tokens += usage.input_tokens as u64;
        entry.output_tokens += usage.output_tokens as u64;
        entry.cache_read_tokens += usage.cache_read_input_tokens as u64;
        entry.cache_write_tokens += usage.cache_write_input_tokens as u64;
        entry.reasoning_tokens += usage.reasoning_tokens as u64;
        entry.cost_usd += cost;
        self.total_cost_usd += cost;
        self.total_api_duration_ms += api_duration_ms;
        self.total_api_duration_without_retries_ms += api_duration_without_retries_ms;
    }

    pub fn add_lines_changed(&mut self, added: u32, removed: u32) {
        self.lines_added += added;
        self.lines_removed += removed;
    }

    /// Produce a snapshot for cost rendering. The wall duration is passed
    /// by the caller, who owns the session start time.
    pub fn summary(&self, wall_duration_ms: u64) -> CostSummary {
        CostSummary {
            total_cost_usd: self.total_cost_usd,
            by_model: self.by_model.clone(),
            total_api_duration_ms: self.total_api_duration_ms,
            total_wall_duration_ms: wall_duration_ms,
            total_api_duration_without_retries_ms: self.total_api_duration_without_retries_ms,
            lines_added: self.lines_added,
            lines_removed: self.lines_removed,
        }
    }
}

// ===== observability log =====

/// Append-only collection of records from every subsystem. The log does not
/// mutate any track state; it only appends and aggregates. The ContextView
/// trait projects its contents as read-only snapshots.
pub struct ObservabilityLog {
    writes: Vec<TurnEvent>,
    tool_calls: Vec<ToolCallRecord>,
    last_report: Option<CompressResult>,
    errors: Vec<HookError>,
    cost: CostAccumulator,
    cumulative_usage: Usage,
    last_turn_delta: Option<TurnTokenDelta>,
    /// Logical turn number (one per drive-loop iteration, NOT per LLM call —
    /// length/overflow retries inside one model_call_stream entry are the same
    /// turn). Incremented by start_turn at the turn boundary, not by record_usage.
    turn_count: u32,
    /// Total provider round-trips this session (every record_turn call,
    /// including length-recovery retries). The billing count — what turn_count
    /// used to wrongly count before the split.
    call_count: u32,
    /// Round-trip index within the current logical turn (1-based; resets at
    /// start_turn). Lets /trajectory show "the turn with its retry count" without correlating against
    /// TruncationVerdict.
    call_in_turn: u32,
    context_window: u32,
    session_start: Instant,
    pricing: Box<dyn PricingTable>,
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
pub fn tool_failure_reason(output: &serde_json::Value) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
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

impl ObservabilityLog {
    /// Create a log with a no-charge pricing table and the given context
    /// window (used for context-fill percentage).
    pub fn new(context_window: u32) -> Self {
        Self::with_pricing(context_window, Box::new(NoCharge))
    }

    /// Create a log with a custom pricing table.
    pub fn with_pricing(context_window: u32, pricing: Box<dyn PricingTable>) -> Self {
        Self {
            writes: Vec::new(),
            tool_calls: Vec::new(),
            last_report: None,
            errors: Vec::new(),
            cost: CostAccumulator::default(),
            cumulative_usage: Usage::default(),
            last_turn_delta: None,
            turn_count: 0,
            call_count: 0,
            call_in_turn: 0,
            context_window,
            session_start: Instant::now(),
            pricing,
        }
    }

    /// Append a trajectory event to the write log. O(1) push.
    pub fn append_write(&mut self, event: TurnEvent) {
        self.writes.push(event);
    }

    /// Record a tool call outcome. Aggregates by tool name: increments
    /// call/fail counts, deduplicates and caps failure reasons, and feeds
    /// the latency histogram.
    pub fn record_tool_call(
        &mut self,
        name: &str,
        duration_ms: u64,
        is_error: bool,
        error: Option<&str>,
    ) {
        let idx = self.tool_calls.iter().position(|r| r.tool_name == name);
        let idx = idx.unwrap_or_else(|| {
            self.tool_calls.push(ToolCallRecord::new(name));
            self.tool_calls.len() - 1
        });
        let rec = &mut self.tool_calls[idx];
        rec.call_count += 1;
        if is_error {
            rec.fail_count += 1;
            if let Some(err) = error {
                let reason = truncate_str(err, REASON_CAP);
                if !rec.failure_reasons.iter().any(|r| r == &reason) {
                    rec.failure_reasons.push(reason);
                    if rec.failure_reasons.len() > FAILURE_REASONS_CAP {
                        rec.failure_reasons.truncate(FAILURE_REASONS_CAP);
                    }
                }
            }
        }
        rec.total_duration_ms += duration_ms;
        rec.last_duration_ms = duration_ms;
        rec.max_duration_ms = rec.max_duration_ms.max(duration_ms);
        rec.latency_buckets.record(duration_ms);
    }

    /// Record per-tool stats (count/fail/reasons) from a turn's result list.
    /// call_names maps result call_id to tool name (the results carry only
    /// call_id; the caller pulls names from the model ToolCall items).
    /// duration_ms is 0 here — per-call timing wires inside the partitioned
    /// executor when latency stats are needed; count + fail are accurate now.
    /// An error-shaped payload (object with an error field) counts as an
    /// error, same convention as the agent loop count_tool_outcomes.
    pub fn record_tool_outcomes(
        &mut self,
        results: &[(String, serde_json::Value)],
        call_names: &std::collections::HashMap<String, String>,
    ) {
        for (id, output) in results {
            let Some(name) = call_names.get(id) else {
                continue;
            };
            let reason = tool_failure_reason(output);
            let err_str: Option<&str> = reason.as_deref();
            self.record_tool_call(name, 0, reason.is_some(), err_str);
        }
    }

    /// Fold a provider response's usage into the per-model cost tally and
    /// derive the per-turn token delta. The cost is computed via the
    /// pricing table held by the log. served_token_count is the local
    /// tiktoken count of what was sent this turn, used as a fallback when
    /// the provider omits usage (common for streaming proxies) — same
    /// provider-primary + local-fallback convention as TruncationVerdict's
    /// output-token pair.
    #[expect(clippy::too_many_arguments, reason = "param grouping deliberate")]
    pub fn record_usage(
        &mut self,
        model: &str,
        usage: &Usage,
        served_token_count: u32,
        api_duration_ms: u64,
        api_duration_without_retries_ms: u64,
        context_window: u32,
        max_output_tokens: u32,
    ) {
        let cost = self.pricing.cost_for(model, usage);
        self.cost.record(
            model,
            usage,
            cost,
            api_duration_ms,
            api_duration_without_retries_ms,
        );
        // Fill the per-model ModelUsage fields that were dead (constant 0).
        // The values share a source with the request body + pre-flight
        // reserve (I11 same-source): the resolved context_window +
        // max_output_tokens the runner used for this call.
        if let Some(entry) = self.cost.by_model.get_mut(model) {
            entry.context_window = context_window;
            entry.max_output_tokens = max_output_tokens;
        }
        self.cumulative_usage.input_tokens += usage.input_tokens;
        self.cumulative_usage.output_tokens += usage.output_tokens;
        self.cumulative_usage.total_tokens += usage.total_tokens;
        self.cumulative_usage.non_cached_input_tokens += usage.non_cached_input_tokens;
        self.cumulative_usage.cache_read_input_tokens += usage.cache_read_input_tokens;
        self.cumulative_usage.cache_write_input_tokens += usage.cache_write_input_tokens;
        self.cumulative_usage.reasoning_tokens += usage.reasoning_tokens;

        // turn_count is NOT incremented here — start_turn (called at the
        // model_call_stream entry, once per drive-loop iteration) owns the
        // logical turn boundary, so length/overflow retries inside one turn
        // do not advance it. call_count + call_in_turn advance per call.
        self.call_count += 1;
        self.call_in_turn += 1;
        let cache_read = usage.cache_read_input_tokens as u64;
        let input = usage.input_tokens as u64;
        // cache_read is already included in input (Usage.input_tokens is
        // inclusive of cache-read and cache-write), so the ratio is cache_read
        // / input — not cache_read / (cache_read + input), which would
        // double-count and roughly halve the ratio. None when input is zero
        // (usage omitted / empty input) so an unknown hit rate is not shown
        // as 0% — same rule as context_pct, the two sit side by side.
        let cache_hit_ratio = if input > 0 {
            Some(cache_read as f64 / input as f64)
        } else {
            None
        };
        // context_pct is the CURRENT served occupancy, not cumulative input
        // (cumulative grows unbounded and false-ceilings). The provider's
        // measured input_tokens is the source of truth — it counted exactly
        // what we sent. The local tiktoken served_token_count covers providers
        // that omit usage (common for streaming proxies); the two numbers
        // serve the same question on opposite sides of the call, mirroring
        // TruncationVerdict's server/self output-token pair. When both are
        // zero the fill is unknown, not 0% — a 0% display for an unknown
        // value is the plausible-but-wrong number class (#73).
        let served_tokens = if usage.input_tokens > 0 {
            usage.input_tokens as u64
        } else {
            served_token_count as u64
        };
        let context_pct = if served_tokens > 0 && self.context_window > 0 {
            Some((served_tokens as f64 / self.context_window as f64).clamp(0.0, 1.0))
        } else {
            None
        };
        self.last_turn_delta = Some(TurnTokenDelta {
            turn: self.turn_count,
            call_in_turn: self.call_in_turn,
            input,
            output: usage.output_tokens as u64,
            cache_read,
            cache_write: usage.cache_write_input_tokens as u64,
            reasoning: usage.reasoning_tokens as u64,
            cumulative: self.cumulative_usage.clone(),
            cache_hit_ratio,
            context_pct,
        });
    }

    /// Mark the start of a logical turn: increment turn_count + reset
    /// call_in_turn. Called once per model_call_stream entry (one drive-loop
    /// iteration = one logical turn), so length/overflow retries inside that
    /// entry stay on the same turn number. The turn boundary is the
    /// model-call entry (one drive-loop iteration = one logical turn).
    pub fn start_turn(&mut self) {
        self.turn_count += 1;
        self.call_in_turn = 0;
    }

    /// The logical turn number + round-trip index within it, for stamping
    /// onto the durable TurnUsage event so /trajectory groups without
    /// deriving boundaries from event order.
    pub(crate) fn turn_coords(&self) -> (u32, u32) {
        (self.turn_count, self.call_in_turn)
    }

    /// Cache the most recent compress result for insight projection.
    pub fn set_report(&mut self, report: CompressResult) {
        self.last_report = Some(report);
    }

    /// Append a hook error to the error stream.
    pub fn record_hook_error(&mut self, err: HookError) {
        self.errors.push(err);
    }

    /// Accumulate code-change counts (lines added/removed).
    pub fn add_lines_changed(&mut self, added: u32, removed: u32) {
        self.cost.add_lines_changed(added, removed);
    }

    /// The most recent per-turn token delta, for live status rendering.
    pub fn last_turn_delta(&self) -> Option<&TurnTokenDelta> {
        self.last_turn_delta.as_ref()
    }

    /// Wall-clock duration since the log was created.
    pub fn wall_duration_ms(&self) -> u64 {
        self.session_start.elapsed().as_millis() as u64
    }

    /// Clear all records. Called on clear so the log reflects the new
    /// session only.
    pub fn reset(&mut self) {
        self.writes.clear();
        self.tool_calls.clear();
        self.last_report = None;
        self.errors.clear();
        self.cost = CostAccumulator::default();
        self.cumulative_usage = Usage::default();
        self.last_turn_delta = None;
        self.turn_count = 0;
        self.call_count = 0;
        self.call_in_turn = 0;
        self.session_start = Instant::now();
    }

    /// Clear the last-turn delta. Called after a mid-turn compaction: the
    /// post-compact served view is rebuilt from the manifest (projection-
    /// accurate), so the pre-compact provider-reported input tokens are no
    /// longer a valid floor for effective_served_tokens. Leaving them stale
    /// floors the estimate to the pre-compact size and false-trips the
    /// pre-flight / overflow gate on a view that is actually under threshold.
    pub fn clear_last_turn_delta(&mut self) {
        self.last_turn_delta = None;
    }
}

/// Read-only projection of the log's aggregates for inspection commands.
/// Renamed from ContextView to MetricsView (2026-08-02): the log no longer
/// duplicates the served view — context_pct is passed to record_usage as a
/// u32 (provider input_tokens primary, local tiktoken fallback), so the
/// breakdown() method + last_breakdown field are gone. trajectory() still
/// reads the writes buffer until /trajectory projects from the store
/// directly. The rename also disambiguates from the TUI's own ContextView
/// struct (the /context view model).
pub trait MetricsView {
    /// The trajectory event stream (append order). Drops when the store
    /// becomes the single source for /trajectory projection.
    fn trajectory(&self) -> &[TurnEvent];
    /// The most recent compress result, for trajectory compact drill-down.
    fn report(&self) -> Option<&CompressResult>;
    /// Per-tool aggregate statistics, for the trajectory tool-filter view.
    fn tool_stats(&self) -> &[ToolCallRecord];
    /// Aggregated cost, for the status bar + cost view.
    fn cost(&self) -> CostSummary;
    /// The most recent per-turn token delta, for the live status bar.
    fn turn_delta(&self) -> Option<&TurnTokenDelta>;
}

impl MetricsView for ObservabilityLog {
    fn trajectory(&self) -> &[TurnEvent] {
        &self.writes
    }

    fn report(&self) -> Option<&CompressResult> {
        self.last_report.as_ref()
    }

    fn tool_stats(&self) -> &[ToolCallRecord] {
        &self.tool_calls
    }

    fn cost(&self) -> CostSummary {
        self.cost.summary(self.wall_duration_ms())
    }

    fn turn_delta(&self) -> Option<&TurnTokenDelta> {
        self.last_turn_delta.as_ref()
    }
}

// ===== export sink =====

/// Error produced by an export sink.
#[derive(Debug)]
pub enum ExportError {
    Io,
    Serialize(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io => write!(f, "export io error"),
            Self::Serialize(msg) => write!(f, "export serialize error: {msg}"),
        }
    }
}

impl std::error::Error for ExportError {}

/// Project the substrate (trajectory, tool stats, cost, errors) into an
/// industry-standard format for analysis or post-training. Multiple sinks
/// serve different consumers; the canonical internal format is JSON.
pub trait ExportSink {
    /// Format identifier ("json", "otel-genai", "hf-messages", etc.).
    fn format_name(&self) -> &str;
    /// Serialize the log contents to the writer.
    fn export(
        &self,
        log: &ObservabilityLog,
        out: &mut dyn std::io::Write,
    ) -> Result<(), ExportError>;
}

// ===== helpers =====

/// Truncate a string to a char budget, appending an overflow marker when
/// cut. Counts chars, not bytes, so multi-byte boundaries are respected.
pub(crate) fn truncate_str(s: &str, cap: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= cap {
        return s.to_string();
    }
    let mut t: String = chars[..cap].iter().collect();
    let extra = chars.len() - cap;
    t.push_str(&format!(" (+{extra} chars)"));
    t
}
