//! Tests for this module.

use super::*;
use houyicoder_core::observability::ModelUsage;
use houyicoder_protocol::llm::UsageProvenance;

#[test]
fn test_model_view_trims_fields() {
    // USD, web search, context window, and max output tokens are dropped:
    // the Usage tab renders token counts only, and capability fields
    // belong to the /model pane, not the usage breakdown.
    let entries = vec![(
        "glm-5.2".to_string(),
        ModelUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 800,
            cache_write_tokens: 0,
            reasoning_tokens: 200,
            web_search_requests: 9,
            cost_usd: 1.5,
            context_window: 200_000,
            max_output_tokens: 8192,
            provenance: UsageProvenance::ALL_REPORTED,
        },
    )];
    let view = build_model_usage_view(entries);
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].model, "glm-5.2");
    assert_eq!(view[0].input_tokens, 1000);
    assert_eq!(view[0].output_tokens, 500);
    assert_eq!(view[0].cache_read_tokens, 800);
    assert_eq!(view[0].cache_write_tokens, 0);
    assert_eq!(view[0].reasoning_tokens, 200);
    assert_eq!(
        view[0].provenance,
        UsageProvenance::ALL_REPORTED,
        "the trim keeps what the row needs to decide whether a share is claimable"
    );
}

#[test]
fn test_model_view_preserves_order() {
    // The caller (Runner::by_model_usage) sorts heaviest-first; the view
    // must not reorder, only trim.
    let entries = vec![
        ("qwen3.7-max".to_string(), ModelUsage::default()),
        ("glm-5.2".to_string(), ModelUsage::default()),
    ];
    let view = build_model_usage_view(entries);
    assert_eq!(
        view.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
        ["qwen3.7-max", "glm-5.2"],
        "order preserved as given"
    );
}
