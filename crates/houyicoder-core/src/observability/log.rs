//! The append-only observation log and its read-only view.
//!
//! The log gathers records from every subsystem (context lifecycle, tool
//! pipeline, hook dispatch, provider responses). It does not mutate any
//! subsystem state — it only appends and aggregates. The MetricsView trait
//! exposes the log contents as read-only snapshots for a host to render
//! inspection commands.

use std::collections::HashMap;
use std::time::Instant;

use houyicoder_api::session::recorded_turn_usage;
use houyicoder_context::SessionLogEntry;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::tool::tool_result_not_run;
use serde_json::Value;

use crate::agent::compaction::RecordedCompaction;
use crate::agent::hook::HookError;

use super::tool_stats::{FAILURE_REASONS_CAP, REASON_CAP, ToolCallRecord, tool_failure_reason};
use super::truncate_str;
use super::usage::{CostAccumulator, CostSummary, NoCharge, PricingTable, TurnTokenDelta};

/// Append-only collection of records from every subsystem. The log does not
/// mutate any track state; it only appends and aggregates. The MetricsView
/// trait exposes its contents as read-only snapshots.
pub struct ObservabilityLog {
    writes: Vec<SessionLogEntry>,
    tool_calls: Vec<ToolCallRecord>,
    last_report: Option<RecordedCompaction>,
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
            cumulative_usage: Usage::identity(),
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
    pub fn append_write(&mut self, event: SessionLogEntry) {
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
    /// duration_ms is 0 here — per-call timing enters the partitioned
    /// executor when latency stats are needed; count + fail are accurate now.
    /// An error-shaped payload (object with an error field) counts as an
    /// error, same convention as the agent loop count_tool_outcomes.
    ///
    /// A call answered without executing it is dropped entirely: a call that
    /// never ran would drag a tool's failure rate without being its failure.
    /// The agent loop count still counts it as a call, because that count
    /// answers what the model asked for; the two totals differ by exactly
    /// the unrun calls.
    pub fn record_tool_outcomes(
        &mut self,
        results: &[(String, Value)],
        call_names: &HashMap<String, String>,
    ) {
        for (id, output) in results {
            let Some(name) = call_names.get(id) else {
                continue;
            };
            // A call the runner answered without executing it is neither a
            // success nor a failure: counting it as either reports a tool
            // that never ran and skews the failure rate it feeds.
            if tool_result_not_run(output) {
                continue;
            }
            let reason = tool_failure_reason(output);
            let err_str: Option<&str> = reason.as_deref();
            self.record_tool_call(name, 0, reason.is_some(), err_str);
        }
    }

    /// Fold a provider response's usage into the per-model cost tally and
    /// derive the per-turn token delta. estimated_input_tokens is the local
    /// tiktoken count of what was sent this turn, used as a fallback when
    /// the provider omits usage.
    #[expect(clippy::too_many_arguments, reason = "param grouping deliberate")]
    pub fn record_usage(
        &mut self,
        model: &str,
        usage: &Usage,
        estimated_input_tokens: u32,
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
        self.cost
            .set_model_limits(model, context_window, max_output_tokens);
        self.cumulative_usage = self.cumulative_usage.saturating_add(usage);

        // turn_count is NOT incremented here — start_turn (called at the
        // model_call_stream entry, once per drive-loop iteration) owns the
        // logical turn boundary, so length/overflow retries inside one turn
        // do not advance it. call_count + call_in_turn advance per call.
        self.call_count += 1;
        self.call_in_turn += 1;
        let cache_read = usage.cache_read_input_tokens as u64;
        let input = usage.input_tokens as u64;
        // The per-turn ratio is the shared cache_hit_rate accessor, so it is
        // provenance-gated the same as the status and context panes: None
        // when input or cache-read went unreported, never a content-free 0%.
        let cache_hit_ratio = usage.cache_hit_rate();
        // context_pct is the current context occupancy, not cumulative
        // input. The provider's measured input_tokens is primary; the local
        // tiktoken estimate covers providers that omit usage. When both
        // are zero the fill is unknown, not 0%.
        let observed_or_estimated_input_tokens = if usage.input_tokens > 0 {
            usage.input_tokens as u64
        } else {
            estimated_input_tokens as u64
        };
        let context_pct = if observed_or_estimated_input_tokens > 0 && self.context_window > 0 {
            Some(
                (observed_or_estimated_input_tokens as f64 / self.context_window as f64)
                    .clamp(0.0, 1.0),
            )
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

    /// Cache the most recent compaction recording for insight reporting.
    pub fn set_report(&mut self, report: RecordedCompaction) {
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

    /// Cumulative usage across every turn this log folded. Test-only: a
    /// reader outside the module uses the per-turn delta or the cost summary,
    /// which cover the same figures with their span made explicit.
    #[cfg(test)]
    pub(super) fn cumulative_usage(&self) -> &Usage {
        &self.cumulative_usage
    }

    /// Wall-clock duration since the log was created.
    pub fn wall_duration_ms(&self) -> u64 {
        self.session_start.elapsed().as_millis() as u64
    }

    /// Clear every record and counter, for a log that starts over from
    /// nothing. A host's /clear path resets the usage figures instead
    /// (reset_usage): it keeps the turn counters, because those number turns
    /// in a log that keeps going rather than figures for one span of it.
    pub fn reset(&mut self) {
        self.writes.clear();
        self.tool_calls.clear();
        self.last_report = None;
        self.errors.clear();
        self.cost = CostAccumulator::default();
        self.cumulative_usage = Usage::identity();
        self.last_turn_delta = None;
        self.turn_count = 0;
        self.call_count = 0;
        self.call_in_turn = 0;
        self.session_start = Instant::now();
    }

    /// Restart the spend figures this log folds: the per-model table, the
    /// cumulative usage, and the last turn delta, which embeds it. The host
    /// calls this on /clear, where the session's tally starts over. The turn
    /// and call counters stay, because they number turns in a log that keeps
    /// going rather than figures for one span of it.
    pub fn reset_usage(&mut self) {
        self.cost = CostAccumulator::default();
        self.cumulative_usage = Usage::identity();
        self.last_turn_delta = None;
    }

    /// Start the spend figures from the turns a session's log already holds,
    /// so a runner that reopened the session reports that session's per-model
    /// rows and cumulative rather than only the turns since it reopened: a
    /// pane reads the cumulative and the rows beside each other, and figures
    /// covering different spans would be read as one session's.
    ///
    /// A record carries counts and their completeness but no cost or
    /// duration, so the restored rows carry counts; the call counters stay
    /// with the run, which numbers the calls it makes.
    pub fn seed_usage_from_events(&mut self, events: &[SessionLogEntry]) {
        for entry in events {
            let Some((model, usage)) = recorded_turn_usage(&entry.event) else {
                continue;
            };
            self.cost.record(model, &usage, 0.0, 0, 0);
            self.cumulative_usage = self.cumulative_usage.saturating_add(&usage);
        }
    }

    /// Clear the last-turn delta. Called after a mid-turn compaction: the
    /// post-compact context is rebuilt from the manifest, so the
    /// pre-compact provider-reported input tokens are no longer a valid
    /// floor for conservative_input_tokens.
    pub fn clear_last_turn_delta(&mut self) {
        self.last_turn_delta = None;
    }
}

/// Read-only view of the log's aggregates for inspection commands. The log
/// does not duplicate the assembled context: context_pct reaches
/// record_usage as a u32 (provider input_tokens primary, local tiktoken
/// fallback), so no breakdown accessor lives here. trajectory() still reads
/// the writes buffer until /trajectory reads the store directly.
pub trait MetricsView {
    /// The trajectory event stream (append order). Drops when the store
    /// becomes the single source /trajectory reads.
    fn trajectory(&self) -> &[SessionLogEntry];
    /// The most recent compaction recording, for trajectory compact drill-down.
    fn report(&self) -> Option<&RecordedCompaction>;
    /// Per-tool aggregate statistics, for the trajectory tool-filter view.
    fn tool_stats(&self) -> &[ToolCallRecord];
    /// Aggregated cost, for the status bar + cost view.
    fn cost(&self) -> CostSummary;
    /// The most recent per-turn token delta, for the live status bar.
    fn turn_delta(&self) -> Option<&TurnTokenDelta>;
}

impl MetricsView for ObservabilityLog {
    fn trajectory(&self) -> &[SessionLogEntry] {
        &self.writes
    }

    fn report(&self) -> Option<&RecordedCompaction> {
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
