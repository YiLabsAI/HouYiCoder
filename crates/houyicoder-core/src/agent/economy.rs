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

/// The projected size of the served view AFTER a compaction (the verbatim
/// tail + the summary). The economy decision runs BEFORE the compaction, so
/// the caller estimates this (e.g. from the manifest's verbatim-tail token
/// count, or a fold-ratio heuristic). A rough estimate is fine — the gate
/// is conservative, and an over-estimate of new under-counts savings
/// (leans toward not compacting, which is safe).
#[derive(Debug, Clone, Copy)]
pub struct CompactProjection {
    /// The current served-token count (pre-compact).
    pub old_tokens: u64,
    /// The projected served-token count after compaction.
    pub new_tokens: u64,
    /// The remaining turns in the run (for the savings horizon). A simple
    /// estimate (max_turns - current_turn) for now; an EWMA projection lands
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

/// Build a compaction projection from the served-token count + remaining
/// turns. The post-compact size is a fold-ratio heuristic (the verbatim tail
/// is roughly a third of the served view); the summarizer cost is the folded
/// span re-billed at input + a tenth of it at output (a summary is ~10% of
/// the input). Rough but conservative — an over-estimate of the post-compact
/// size under-counts savings, leaning toward skipping (safe; the ceiling
/// gate still fires if the view keeps growing).
pub fn economy_projection(served_tokens: u32, remaining_turns: u64) -> CompactProjection {
    let projected_new = served_tokens / 3;
    let folded = served_tokens.saturating_sub(projected_new);
    CompactProjection {
        old_tokens: served_tokens as u64,
        new_tokens: projected_new as u64,
        remaining_turns,
        summarizer_input_tokens: folded as u64,
        summarizer_output_tokens: (folded / 10) as u64,
    }
}

/// Decide whether a compaction is economically worthwhile given the
/// projection + the provider's cost rates. The decision uses:
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
pub fn economy_decision(projection: CompactProjection, cost: &ProviderCost) -> EconomyDecision {
    let old = projection.old_tokens as f64;
    let new = projection.new_tokens as f64;
    let remaining = projection.remaining_turns as f64;

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
            summarizer_cost: summarizer_cost(projection, cost),
            clear_at_least: f64::INFINITY,
        };
    }

    // The savings: the cleared prefix is re-read cached (at the discount)
    // instead of base-billed, each remaining turn.
    let cache_read_saving = cost.cache_read_saving_per_token();
    let projected_savings = cache_read_saving * shrink * remaining;
    // The rewrite: re-writing the compacted prefix at the write premium.
    let rewrite_cost = cost.cache_write_5m * new;
    let summarizer_cost = summarizer_cost(projection, cost);
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
fn summarizer_cost(projection: CompactProjection, cost: &ProviderCost) -> f64 {
    (projection.summarizer_input_tokens as f64) * cost.input
        + (projection.summarizer_output_tokens as f64) * cost.output
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_api::cost_model::{AnthropicCostModel, CostModelProvider};

    fn anthropic() -> ProviderCost {
        AnthropicCostModel.cost()
    }

    #[test]
    fn test_economy_projection_fold_ratio() {
        // The projection folds the served view by a third; the summarizer
        // re-bills the folded span at input + a tenth at output.
        let p = economy_projection(150_000, 10);
        assert_eq!(p.old_tokens, 150_000);
        assert_eq!(p.new_tokens, 50_000);
        assert_eq!(p.summarizer_input_tokens, 100_000);
        assert_eq!(p.summarizer_output_tokens, 10_000);
        assert_eq!(p.remaining_turns, 10);
    }

    #[test]
    fn test_no_shrink_skips() {
        // old == new: nothing to save, skip.
        let dec = economy_decision(
            CompactProjection {
                old_tokens: 100_000,
                new_tokens: 100_000,
                remaining_turns: 10,
                summarizer_input_tokens: 0,
                summarizer_output_tokens: 0,
            },
            &anthropic(),
        );
        assert!(!dec.compact);
        assert_eq!(dec.reason, EconomyReason::NoShrink);
    }

    #[test]
    fn test_no_horizon_skips() {
        // remaining_turns 0: a compact is always a loss (nothing left to save).
        let dec = economy_decision(
            CompactProjection {
                old_tokens: 100_000,
                new_tokens: 30_000,
                remaining_turns: 0,
                summarizer_input_tokens: 0,
                summarizer_output_tokens: 0,
            },
            &anthropic(),
        );
        assert!(!dec.compact);
        assert_eq!(dec.reason, EconomyReason::NoHorizon);
    }

    #[test]
    fn test_economy_compacts_on_savings() {
        // A large shrink over many turns: the cached-read savings exceed the
        // rewrite + summarizer cost. Compact.
        let dec = economy_decision(
            CompactProjection {
                old_tokens: 150_000,
                new_tokens: 30_000,
                remaining_turns: 10,
                summarizer_input_tokens: 30_000,
                summarizer_output_tokens: 2_000,
            },
            &anthropic(),
        );
        assert!(
            dec.compact,
            "should compact: savings={} cost={}",
            dec.projected_savings,
            dec.rewrite_cost + dec.summarizer_cost
        );
        assert_eq!(dec.reason, EconomyReason::Economy);
        // projected_savings = 0.9 × 120_000 × 10 = 1_080_000.
        assert!((dec.projected_savings - 1_080_000.0).abs() < 1e-6);
        // rewrite_cost = 1.25 × 30_000 = 37_500.
        assert!((dec.rewrite_cost - 37_500.0).abs() < 1e-6);
        // summarizer_cost = 30_000 × 1.0 + 2_000 × 5.0 = 40_000.
        assert!((dec.summarizer_cost - 40_000.0).abs() < 1e-6);
    }

    #[test]
    fn test_short_horizon_skips_breakeven() {
        // The savings cover the rewrite (shrink >= clear_at_least) but not
        // the rewrite + summarizer (the summarizer cost pushes total above
        // savings). This is the BelowBreakeven case: compaction would
        // recover the rewrite but not the summary's own cost.
        let dec = economy_decision(
            CompactProjection {
                old_tokens: 40_000,
                new_tokens: 30_000,
                remaining_turns: 5,
                summarizer_input_tokens: 10_000,
                summarizer_output_tokens: 1_000,
            },
            &anthropic(),
        );
        assert!(!dec.compact, "savings below total cost should skip");
        // projected_savings = 0.9 × 10_000 × 5 = 45_000; rewrite 37_500;
        // summarizer 15_000; total 52_500. 45_000 < 52_500 → skip. The shrink
        // (10_000) >= clear_at_least (8_333) so it is BelowBreakeven, not the
        // clear floor.
        assert_eq!(dec.reason, EconomyReason::BelowBreakeven);
    }

    #[test]
    fn test_clear_at_least_derived() {
        // clear_at_least = rewrite_cost / (cache_read_saving × remaining).
        // Verify the derivation matches the formula (no magic constant).
        let dec = economy_decision(
            CompactProjection {
                old_tokens: 150_000,
                new_tokens: 30_000,
                remaining_turns: 10,
                summarizer_input_tokens: 0,
                summarizer_output_tokens: 0,
            },
            &anthropic(),
        );
        let expected_clear = 37_500.0 / (0.9 * 10.0); // rewrite / (saving × remaining)
        assert!(
            (dec.clear_at_least - expected_clear).abs() < 1e-6,
            "clear_at_least derived: {} vs {}",
            dec.clear_at_least,
            expected_clear
        );
    }

    #[test]
    fn test_thin_shrink_below_floor() {
        // A shrink below the clear_at_least floor is a net loss: even though
        // the breakeven math might pass (with a huge horizon), the floor
        // blocks a compaction that clears too little to recover the rewrite.
        let dec = economy_decision(
            CompactProjection {
                old_tokens: 30_100,
                new_tokens: 30_000,
                remaining_turns: 100,
                summarizer_input_tokens: 0,
                summarizer_output_tokens: 0,
            },
            &anthropic(),
        );
        // shrink = 100; clear_at_least = 1.25×30_000 / (0.9×100) = 416.7.
        // 100 < 416.7 → below the floor.
        assert!(!dec.compact, "thin shrink below clear floor should skip");
        assert_eq!(dec.reason, EconomyReason::BelowClearFloor);
    }
}
