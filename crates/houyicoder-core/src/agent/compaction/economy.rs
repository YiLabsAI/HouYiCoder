//! Economy-driven compaction decision: weigh projected cache savings against
//! rewrite + summarizer cost so compaction fires only when it saves money.
//! The ceiling-driven trigger (the absolute pre-flight buffer) always fires
//! regardless of cost — bricking is more expensive than any compact. The
//! economy gate runs BEFORE the ceiling to compact proactively when the
//! remaining turns make it worthwhile, and skips when a compact would cost
//! more than it saves (a short remaining horizon makes the rewrite a net
//! loss).
//!
//! The clear_at_least floor is DERIVED from the same rates, not a separate
//! magic constant: clear_at_least = rewrite_cost / (cache_read_saving ×
//! remaining_turns). This keeps the breakeven gate and the clear floor
//! consistent — a separate magic floor could disagree with the breakeven
//! (one passes, the other blocks), contradicting the decision.

use houyicoder_api::cost_model::ProviderCost;

/// The projected size of the assembled context AFTER a compaction (the
/// verbatim tail + the summary). The economy decision runs BEFORE the
/// compaction, so the caller estimates this. A rough estimate is fine —
/// the gate is conservative, and an over-estimate of new under-counts
/// savings (leans toward not compacting, which is safe).
#[derive(Debug, Clone, Copy)]
pub struct CompactionEstimate {
    /// The current input-token count (pre-compact).
    pub old_tokens: u64,
    /// The projected input-token count after compaction.
    pub new_tokens: u64,
    /// The remaining turns in the run (for the savings horizon). A simple
    /// estimate (max_turns - current_turn) for now; an EWMA estimate lands
    /// when the proactive-lookahead wires.
    pub remaining_turns: u64,
    /// The summarizer's input-token cost (the LLM call to produce the summary
    /// re-bills the folded span).
    pub summarizer_input_tokens: u64,
    /// The summarizer's output-token cost (the summary text length).
    pub summarizer_output_tokens: u64,
}

/// The economy decision: whether a compaction is economically worthwhile,
/// the cost breakdown, and the clear_at_least floor. The decision is
/// Economy (compact, it saves), Skip (not worth it, remaining horizon too
/// short or savings too thin), or Ceiling (the caller already decided via
/// the absolute buffer — this struct is not produced for that path).
#[derive(Debug, Clone, PartialEq)]
pub struct EconomyDecision {
    pub compact: bool,
    pub reason: EconomyReason,
    pub projected_savings: f64,
    pub rewrite_cost: f64,
    pub summarizer_cost: f64,
    /// The minimum tokens a compaction must clear to break even. DERIVED from
    /// the rates + remaining_turns, not a magic constant. A compaction that
    /// clears fewer than this is a net loss (the rewrite cost exceeds the
    /// per-turn savings over the remaining horizon).
    pub clear_at_least: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EconomyReason {
    /// Projected savings exceed rewrite + summarizer cost over the horizon.
    Economy,
    /// Savings do not cover the rewrite + summarizer cost (skip).
    BelowBreakeven,
    /// The clear_at_least floor was not met (a compaction would not clear
    /// enough to break even).
    BelowClearFloor,
    /// No remaining turns — a compact is always a loss (nothing left to save).
    NoHorizon,
    /// The compaction does not shrink the view (old == new) — nothing to save.
    NoShrink,
}

/// Build a compaction estimate from the current input-token count +
/// remaining turns. The post-compact size is a fold-ratio heuristic (the
/// verbatim tail is roughly a third of the assembled context); the
/// summarizer cost is the folded span re-billed at input + a tenth at
/// output. Rough but conservative — an over-estimate under-counts
/// savings, leaning toward skipping (safe; the ceiling gate still fires
/// if the view keeps growing).
pub fn estimate_compaction(current_input_tokens: u32, remaining_turns: u64) -> CompactionEstimate {
    let projected_new = current_input_tokens / 3;
    let folded = current_input_tokens.saturating_sub(projected_new);
    CompactionEstimate {
        old_tokens: current_input_tokens as u64,
        new_tokens: projected_new as u64,
        remaining_turns,
        summarizer_input_tokens: folded as u64,
        summarizer_output_tokens: (folded / 10) as u64,
    }
}

/// Decide whether a compaction is economically worthwhile given the
/// estimate + the provider's cost rates. The decision uses:
/// - projected_savings = cache_read_rate × (old − new) × remaining_turns
///   (the cached prefix is re-read at the discount each remaining turn
///   instead of re-billed at base).
/// - rewrite_cost = cache_write_rate × new (re-writing the compacted prefix
///   at the write premium).
/// - summarizer_cost = summarizer_input × input_rate + summarizer_output ×
///   output_rate (the LLM call to produce the summary).
/// - clear_at_least = rewrite_cost / (cache_read_saving_per_token ×
///   remaining_turns) — DERIVED, so it stays consistent with the breakeven.
///
/// The gate is conservative: any under-estimate of savings (a high new, a
/// short horizon) leans toward Skip, which is safe (the ceiling trigger still
/// fires if the view keeps growing). A zero remaining_turns or zero shrink
/// always skips.
pub fn economy_decision(estimate: CompactionEstimate, cost: &ProviderCost) -> EconomyDecision {
    let old = estimate.old_tokens as f64;
    let new = estimate.new_tokens as f64;
    let remaining = estimate.remaining_turns as f64;

    let shrink = (old - new).max(0.0);
    if shrink <= 0.0 {
        return EconomyDecision {
            compact: false,
            reason: EconomyReason::NoShrink,
            projected_savings: 0.0,
            rewrite_cost: 0.0,
            summarizer_cost: 0.0,
            clear_at_least: 0.0,
        };
    }
    if remaining <= 0.0 {
        return EconomyDecision {
            compact: false,
            reason: EconomyReason::NoHorizon,
            projected_savings: 0.0,
            rewrite_cost: cost.cache_write_5m * new,
            summarizer_cost: summarizer_cost(estimate, cost),
            clear_at_least: f64::INFINITY,
        };
    }

    // The savings: the cleared prefix is re-read cached (at the discount)
    // instead of base-billed, each remaining turn.
    let cache_read_saving = cost.cache_read_saving_per_token();
    let projected_savings = cache_read_saving * shrink * remaining;
    // The rewrite: re-writing the compacted prefix at the write premium.
    let rewrite_cost = cost.cache_write_5m * new;
    let summarizer_cost = summarizer_cost(estimate, cost);
    let total_cost = rewrite_cost + summarizer_cost;

    // The clear_at_least floor: the minimum shrink that breaks even on the
    // rewrite cost over the horizon. DERIVED from the same rates so it
    // cannot disagree with the breakeven gate.
    let clear_at_least = (rewrite_cost / (cache_read_saving * remaining)).max(0.0);

    let compact = projected_savings > total_cost && shrink >= clear_at_least;
    let reason = if !compact {
        if shrink < clear_at_least {
            EconomyReason::BelowClearFloor
        } else {
            EconomyReason::BelowBreakeven
        }
    } else {
        EconomyReason::Economy
    };
    EconomyDecision {
        compact,
        reason,
        projected_savings,
        rewrite_cost,
        summarizer_cost,
        clear_at_least,
    }
}

/// The summarizer's cost: the LLM call to produce the summary re-bills the
/// folded span at the input rate + the summary text at the output rate.
fn summarizer_cost(estimate: CompactionEstimate, cost: &ProviderCost) -> f64 {
    (estimate.summarizer_input_tokens as f64) * cost.input
        + (estimate.summarizer_output_tokens as f64) * cost.output
}

#[cfg(test)]
mod tests;
