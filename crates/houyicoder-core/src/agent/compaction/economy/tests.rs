//! Tests for this module.

use super::*;
use houyicoder_api::cost_model::{AnthropicCostModel, CostModelProvider};

fn anthropic() -> ProviderCost {
    AnthropicCostModel.cost()
}

#[test]
fn test_estimate_fold_ratio() {
    // The estimate folds the assembled context by a third; the
    // summarizer re-bills the folded span at input + a tenth at output.
    let p = estimate_compaction(150_000, 10);
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
        CompactionEstimate {
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
        CompactionEstimate {
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
        CompactionEstimate {
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
        CompactionEstimate {
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
        CompactionEstimate {
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
        CompactionEstimate {
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
