//! Token and cost accounting: the per-turn delta, the per-model breakdown, the
//! pricing port, and the accumulator that folds them.
//!
//! Token semantics follow the protocol layer Usage struct: inclusive totals
//! (input includes cached, output includes reasoning) plus a non-overlapping
//! breakdown. Cost aggregation tracks per-model usage and USD, matching the
//! ModelUsage shape field for field.

use std::collections::HashMap;

use houyicoder_protocol::llm::{Usage, UsageProvenance};
use serde::{Deserialize, Serialize};

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
    /// The per-turn cache hit ratio, taken from Usage.cache_hit_rate so this
    /// field never re-derives the formula the status and context panes
    /// share. Some(0.0) is a confirmed zero hit (real input, no cache); None
    /// is unknown — input zero, or a cache account the endpoint did not fully
    /// report — never a plausible-looking 0%.
    pub cache_hit_ratio: Option<f64>,
    /// Current context occupancy: the provider's measured input_tokens over
    /// context_window, clamped to [0.0, 1.0]. Falls back to the local
    /// tiktoken estimate when the provider omits usage. None when both are
    /// zero, so an unknown fill is never displayed as 0%. Per-turn, not
    /// cumulative — cumulative.input grows unbounded and would false-ceiling.
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
    /// Which of the token counts above reported. A model whose calls never
    /// reported the cache-read field leaves the count at zero, so a reader
    /// consults this before deriving a share from the two counts. Defaults to
    /// all-unreported, so a payload that carries no flag for it reads as
    /// unknown rather than as a measured zero.
    #[serde(default)]
    pub provenance: UsageProvenance,
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

/// Per-model pricing rates (USD per call). A real table is plugged in a later
/// phase; the trait lets the log compute cost without hard-coding rates.
pub trait PricingTable: Send + Sync {
    /// Compute the USD cost for one model call's usage. Returns 0.0 for
    /// unknown models; the host can surface a warning separately.
    fn cost_for(&self, model: &str, usage: &Usage) -> f64;
}

/// A pass-through table that charges nothing. Used in tests and as a
/// placeholder until a real pricing table is plugged in.
pub struct NoCharge;

impl PricingTable for NoCharge {
    fn cost_for(&self, _model: &str, _usage: &Usage) -> f64 {
        0.0
    }
}

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
    /// Fold one response's usage and cost into the per-model tally. The
    /// model's provenance folds with the counts, so one call that did not
    /// report a field leaves the model total short of a full account and a
    /// reader derives no share from it.
    pub fn record(
        &mut self,
        model: &str,
        usage: &Usage,
        cost: f64,
        api_duration_ms: u64,
        api_duration_without_retries_ms: u64,
    ) {
        let entry = self
            .by_model
            .entry(model.to_string())
            .or_insert_with(|| ModelUsage {
                provenance: UsageProvenance::ALL_REPORTED,
                ..ModelUsage::default()
            });
        entry.provenance = entry.provenance.merge(usage.provenance);
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

    /// Stamp the resolved context window and output limit this call ran
    /// with onto the model's row, so a cost report reads the limits the
    /// request was built from rather than a constant.
    pub(super) fn set_model_limits(
        &mut self,
        model: &str,
        context_window: u32,
        max_output_tokens: u32,
    ) {
        if let Some(entry) = self.by_model.get_mut(model) {
            entry.context_window = context_window;
            entry.max_output_tokens = max_output_tokens;
        }
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
