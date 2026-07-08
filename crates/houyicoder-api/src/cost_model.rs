//! Per-provider token cost model. A cost model gives the relative token-cost
//! rates (input, output, cache read, cache write) so the compaction decision
//! layer can weigh projected cache savings against rewrite + summarizer cost.
//! The rates are multipliers relative to the base input rate (input = 1.0),
//! so the decision is provider-portable without a billing table — only the
//! ratios matter for the breakeven math.
//!
//! Anthropic prompt-cache pricing (publicly documented): cache reads
//! discount to ~0.1x base, 5m writes premium ~1.25x, 1h writes
//! premium ~2.0x. OpenAI cached-input discounts vary (50-90%); a
//! provider overrides cost() with its own rates. The default impl
//! carries the Anthropic rates; a multi-provider runtime swaps the
//! model per active provider.

use houyicoder_protocol::llm::Usage;

/// Per-provider token-cost rates, as multipliers relative to the base input
/// rate (input = 1.0). Only the ratios drive the breakeven math, so the model
/// is provider-portable without a billing table. A future USD table lands
/// when per-provider pricing feeds in; the decision logic stays unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderCost {
    /// Base input token rate (input = 1.0).
    pub input: f64,
    /// Output token rate (the output ~5x input).
    pub output: f64,
    /// Cache-read rate (cached prefix reads at a discount; Anthropic ~0.1x).
    pub cache_read: f64,
    /// 5-minute cache-write rate (writing the cache at a premium; ~1.25x).
    pub cache_write_5m: f64,
    /// 1-hour cache-write rate (a longer-TTL write at a higher premium; ~2.0x).
    pub cache_write_1h: f64,
}

impl ProviderCost {
    /// The cache-read saving per token: the difference between a base input
    /// read and a cached read. This is the per-token saving a compacted prefix
    /// earns each remaining turn (the prefix is read cached instead of base).
    pub fn cache_read_saving_per_token(&self) -> f64 {
        self.input - self.cache_read
    }
}

/// A per-provider cost model. The cost() rates drive the breakeven + clear-
/// at-least derivation; effective_input_tokens normalizes split vs subset
/// accounting so cache is never double-counted. The split model reports
/// input_tokens as the uncached remainder with cache fields
/// alongside (Anthropic); the subset model folds cache into
/// input_tokens (OpenAI). The default effective_input_tokens
/// matches the standalone normalizer the pre-flight floor uses.
pub trait CostModelProvider: Send + Sync {
    /// The relative token-cost rates for this provider.
    fn cost(&self) -> ProviderCost;

    /// The effective input-token count for a turn's usage, normalized across
    /// split-accounting (cache fields summed) and subset-accounting (input
    /// already includes cache) reporting. The default matches the standalone
    /// normalizer; a provider overrides for a non-standard reporting shape.
    fn effective_input_tokens(&self, usage: &Usage) -> f64 {
        let split = usage.non_cached_input_tokens as f64
            + usage.cache_read_input_tokens as f64
            + usage.cache_write_input_tokens as f64;
        if split > 0.0 {
            split
        } else {
            usage.input_tokens as f64
        }
    }
}

/// The default Anthropic-pricing cost model. Carries the documented
/// prompt-cache rates (cache read 0.1x, 5m write 1.25x, 1h write 2.0x).
/// Used when no provider-specific model is wired; a multi-provider runtime
/// swaps per active provider.
#[derive(Debug, Default)]
pub struct AnthropicCostModel;

impl CostModelProvider for AnthropicCostModel {
    fn cost(&self) -> ProviderCost {
        ProviderCost {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write_5m: 1.25,
            cache_write_1h: 2.0,
        }
    }
}

/// A type-erased cost-model handle the runner holds. Arc so the shared runner
/// (server + TUI) shares one instance.
pub type SharedCostModel = Arc<dyn CostModelProvider>;

use std::sync::Arc;

#[cfg(test)]
mod tests {
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
}
