//! Tests for this module.

use super::super::*;
use houyicoder_api::cache_policy::{AutoCachePolicy, CachePolicy, NoCachePolicy};
use houyicoder_api::cost_model::AnthropicCostModel;

fn runner() -> Runner {
    Runner::new(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig::default(),
    )
}

#[test]
fn test_default_runner_uses_policy() {
    let r = runner();
    assert_eq!(r.cache_policy.policy(), CachePolicy::Auto);
}

#[test]
fn test_with_cache_policy_overrides() {
    // with_cache_policy swaps the default Auto for NoCachePolicy. Covers
    // the setter (otherwise dead in the default-Auto path).
    let r = runner().with_cache_policy(std::sync::Arc::new(NoCachePolicy));
    assert_eq!(r.cache_policy.policy(), CachePolicy::None);
    // Auto round-trips too.
    let r2 = runner().with_cache_policy(std::sync::Arc::new(AutoCachePolicy));
    assert_eq!(r2.cache_policy.policy(), CachePolicy::Auto);
}

#[test]
fn test_with_cost_model_overrides() {
    // with_cost_model swaps the default Anthropic rates. Covers the
    // setter (otherwise dead in the default-cost path).
    let r = runner().with_cost_model(std::sync::Arc::new(AnthropicCostModel));
    let cost = r.cost_model.cost();
    assert!(
        (cost.cache_read - 0.1).abs() < 1e-9,
        "Anthropic cache_read 0.1x"
    );
}

/// The compact summary preview truncates at 80 chars. The truncation is
/// char-safe (char_indices) so a multi-byte CJK/emoji summary whose 80th
/// byte landed mid-char does not panic -- the prior byte slice did.
#[test]
fn test_truncate_summary_preview_safe() {
    // ASCII > 80 chars: truncated + ellipsis + quoted.
    let long = "x".repeat(100);
    let p = truncate_summary_preview(Some(&long));
    assert!(p.contains('…') && p.starts_with('"'), "truncated: {p}");
    // Short: no truncation.
    assert_eq!(truncate_summary_preview(Some("short")), "\"short\"");
    // None: empty preview.
    assert_eq!(truncate_summary_preview(None), "");
    // CJK > 80 chars (= 300 bytes): truncates WITHOUT panic (char-safe).
    // The prior byte slice at 80 would panic here (byte 80 is mid-char).
    let cjk = "字".repeat(100);
    let p = truncate_summary_preview(Some(&cjk));
    assert!(p.contains('…'), "CJK truncated without panic: {p}");
}
