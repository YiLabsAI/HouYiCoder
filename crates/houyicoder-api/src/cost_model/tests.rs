//! Tests for this module.

use super::*;

#[test]
fn test_rates_match_documented_pricing() {
    let cost = AnthropicCostModel.cost();
    assert!((cost.input - 1.0).abs() < 1e-9, "input 1.0x");
    assert!((cost.cache_read - 0.1).abs() < 1e-9, "cache read 0.1x");
    assert!((cost.cache_write_5m - 1.25).abs() < 1e-9, "5m write 1.25x");
    assert!((cost.cache_write_1h - 2.0).abs() < 1e-9, "1h write 2.0x");
    // The cache-read saving per token = input - cache_read = 0.9.
    assert!(
        (cost.cache_read_saving_per_token() - 0.9).abs() < 1e-9,
        "0.9 saving per cached token"
    );
}

#[test]
fn test_effective_input_includes_cache() {
    // Split model (Anthropic): input_tokens is the uncached
    // remainder; cache read + write are separate.. Effective = the inclusive total.
    let usage = Usage {
        input_tokens: 4_000,
        output_tokens: 1_000,
        total_tokens: 5_000,
        non_cached_input_tokens: 1_500,
        cache_read_input_tokens: 2_000,
        cache_write_input_tokens: 500,
        reasoning_tokens: 0,
    };
    let model = AnthropicCostModel;
    assert!((model.effective_input_tokens(&usage) - 4_000.0).abs() < 1e-9);
}

#[test]
fn test_subset_no_double_count() {
    // Subset model (OpenAI): cache fields zero,
    // input_tokens already includes
    // cached. Effective = input_tokens (do not re-add cache).
    let usage = Usage {
        input_tokens: 6_000,
        output_tokens: 500,
        total_tokens: 6_500,
        non_cached_input_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    let model = AnthropicCostModel;
    assert!((model.effective_input_tokens(&usage) - 6_000.0).abs() < 1e-9);
}
