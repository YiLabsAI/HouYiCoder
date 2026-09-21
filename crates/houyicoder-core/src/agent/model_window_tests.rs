use super::*;

#[test]
fn test_long_suffix_case_insensitive() {
    assert!(has_long_context_suffix("claude-sonnet[1m]"));
    assert!(has_long_context_suffix("claude-sonnet[1M]"));
    assert!(has_long_context_suffix("glm[1m]-beta"));
    assert!(!has_long_context_suffix("claude-sonnet"));
    assert!(!has_long_context_suffix("[2m]")); // [2m] is not a 1m opt-in
}

#[test]
fn test_resolve_suffix_overrides_catalog() {
    // [1m] suffix wins over the catalog (GLM would be 1M from the catalog
    // too, but the suffix is the authoritative opt-in path).
    assert_eq!(resolve_context_window("glm-5.2[1m]"), LONG_CONTEXT_WINDOW);
    assert_eq!(resolve_context_window("anything[1m]"), LONG_CONTEXT_WINDOW);
}

#[test]
fn test_glm5p2_window_one_million() {
    // GLM-5.2 is the first GLM with a real 1M window.
    assert_eq!(resolve_context_window("glm-5.2"), GLM_5_2_CONTEXT_WINDOW);
    assert_eq!(
        resolve_context_window("GLM-5.2-flash"),
        GLM_5_2_CONTEXT_WINDOW
    );
    // glm-52 / glm-5p2 aliases removed from the catalog (simplified).
    // Only the canonical "glm-5.2" spelling is matched.
    assert_eq!(resolve_context_window("glm-52"), DEFAULT_CONTEXT_WINDOW);
    assert_eq!(resolve_context_window("glm-5p2"), DEFAULT_CONTEXT_WINDOW);
}

#[test]
fn test_glm5_two_hundred_k() {
    // GLM-5 / GLM-5.1 serve a 200K window; the old one-liner wrongly
    // returned 1M for every GLM, a context-ceiling-never-brick hazard
    // (over-reporting the window lets the gate overflow).
    assert_eq!(resolve_context_window("glm-5"), GLM_5_CONTEXT_WINDOW);
    assert_eq!(resolve_context_window("glm-5.1"), GLM_5_CONTEXT_WINDOW);
}

#[test]
fn test_unlisted_falls_to_default() {
    // An id outside the catalog — a superseded family, or one this build
    // has no entry for — falls to the conservative 200K default rather
    // than over-reporting a window the provider will not serve. The
    // error-response learner corrects an over-estimate the first time the
    // provider enforces the real limit.
    assert_eq!(resolve_context_window("unknown-a"), DEFAULT_CONTEXT_WINDOW);
    assert_eq!(resolve_context_window("unknown-b"), DEFAULT_CONTEXT_WINDOW);
}

#[test]
fn test_learned_overrides_catalog_suffix() {
    // A provider-enforced limit (recorded from a context-overflow body)
    // is ground truth: it overrides the static catalog AND the [1m]
    // opt-in, because a provider that will not serve 1M cannot be opted
    // into it. Uses a unique model id so the global learned store does
    // not collide with other tests.
    let model = "provider-enforced-test-model";
    record_learned_context_window(model, Some(50_000));
    assert_eq!(resolve_context_window(model), 50_000);
    assert_eq!(
        resolve_context_window(&format!("{model}[1m]")),
        50_000,
        "learned enforced limit wins over the 1m opt-in"
    );
    // A None limit (the body carried no number) does not clobber the
    // learned value — the catalog is left as-is.
    record_learned_context_window(model, None);
    assert_eq!(resolve_context_window(model), 50_000);
}

#[test]
fn test_resolve_window_unknown_conservative() {
    // An unknown model never over-reports; the default is the conservative
    // 200k so the gate errs toward compacting (context-ceiling-never-brick).
    assert_eq!(
        resolve_context_window("some-internal-model"),
        DEFAULT_CONTEXT_WINDOW
    );
    assert_eq!(
        resolve_context_window("acme-coder-7b"),
        DEFAULT_CONTEXT_WINDOW
    );
}

#[test]
fn test_qwen3_family_window_output() {
    // qwen3.7-max has 1M context + 131K output (official docs).
    // qwen3-max entry removed (user confirmed it's not used).
    // qwen3.6-flash has 128K + 8K output.
    assert_eq!(resolve_context_window("qwen3.7-max"), 1_000_000);
    assert_eq!(resolve_max_output_tokens("qwen3.7-max"), 131_072);
    assert_eq!(resolve_context_window("qwen3.7-plus"), 1_000_000);
    assert_eq!(resolve_context_window("qwen3.6-flash"), 131_072);
    assert_eq!(resolve_max_output_tokens("qwen3.6-flash"), 8_192);
    assert_eq!(
        resolve_context_window("qwen3-coder"),
        DEFAULT_CONTEXT_WINDOW
    );
}

#[test]
fn test_deepseek_family_window_output() {
    // deepseek-v4-pro and deepseek-v4-flash both have 1M context + 384K
    // max output (verified from DeepSeek API docs).
    assert_eq!(resolve_context_window("deepseek-v4-pro"), 1_000_000);
    assert_eq!(resolve_max_output_tokens("deepseek-v4-pro"), 384_000);
    assert_eq!(resolve_context_window("deepseek-v4-flash"), 1_000_000);
    assert_eq!(resolve_max_output_tokens("deepseek-v4-flash"), 384_000);
    assert_eq!(
        resolve_context_window("deepseek-chat"),
        DEFAULT_CONTEXT_WINDOW
    );
}

#[test]
fn test_deepseek_effort_dialect() {
    // deepseek-v4 maps to the reasoning branch; the legacy chat name does not.
    assert_eq!(
        effort_dialect("deepseek-v4-pro"),
        EffortDialect::OpenaiReasoning
    );
    assert_eq!(
        effort_dialect("deepseek-v4-pro-0813"),
        EffortDialect::OpenaiReasoning
    );
    assert_eq!(
        effort_dialect("deepseek-v4-flash"),
        EffortDialect::OpenaiReasoning
    );
    assert_eq!(
        dialect_effort_levels("deepseek-v4-pro").to_vec(),
        vec![EffortLevel::Low, EffortLevel::High, EffortLevel::Max]
    );
    assert_eq!(effort_dialect("deepseek-chat"), EffortDialect::NotSupported);
}

#[test]
fn test_reasoning_family_unlisted_window() {
    // No OpenAI reasoning model carries a window entry; an effort-recognized
    // id with no catalog row falls to the conservative default.
    assert_eq!(resolve_context_window("gpt-5"), DEFAULT_CONTEXT_WINDOW);
}

#[test]
fn test_unknown_falls_to_defaults() {
    // A model no family pattern matches falls to both constants.
    assert_eq!(
        resolve_context_window("acme-obscure-id"),
        DEFAULT_CONTEXT_WINDOW
    );
    assert_eq!(
        resolve_max_output_tokens("acme-obscure-id"),
        DEFAULT_MAX_OUTPUT_TOKENS
    );
}

#[test]
fn test_glm_no_output_override() {
    // GLM rows ship no max_output_tokens (the catalog only covers
    // qwen3/deepseek/openai-reasoning for output); GLM falls to the
    // output-token default while keeping its per-version context window.
    assert_eq!(resolve_context_window("glm-5.2"), 1_000_000);
    assert_eq!(
        resolve_max_output_tokens("glm-5.2"),
        DEFAULT_MAX_OUTPUT_TOKENS
    );
}

#[test]
fn test_resolve_reads_no_env() {
    // resolve_* reads no process env: the family catalog + constants are
    // the only authority.
    assert_eq!(resolve_context_window("qwen3.7-max"), 1_000_000);
    assert_eq!(resolve_max_output_tokens("qwen3.7-max"), 131_072);
}

#[test]
fn test_normalize_strips_1m_suffix() {
    assert_eq!(
        normalize_model_for_api("claude-sonnet[1m]"),
        "claude-sonnet"
    );
    assert_eq!(normalize_model_for_api("glm-5.2[2M]"), "glm-5.2");
    assert_eq!(normalize_model_for_api("claude-sonnet"), "claude-sonnet");
    // The suffix may appear mid-id.
    assert_eq!(
        normalize_model_for_api("prefix[1m]-suffix"),
        "prefix-suffix"
    );
}

#[test]
fn test_split_accounting_sums_cache() {
    // Anthropic split: input_tokens is the uncached remainder; cache read
    // + cache creation are separate. Effective = the inclusive total.
    let usage = Usage {
        input_tokens: 4_000,
        output_tokens: 1_000,
        total_tokens: 5_000,
        non_cached_input_tokens: 1_500,
        cache_read_input_tokens: 2_000,
        cache_write_input_tokens: 500,
        reasoning_tokens: 0,
    };
    assert_eq!(effective_input_tokens(&usage), 4_000); // 1500 + 2000 + 500
}

#[test]
fn test_subset_accounting_no_double() {
    // OpenAI subset: cache fields are zero, input_tokens already includes
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
    assert_eq!(effective_input_tokens(&usage), 6_000);
}

#[test]
fn test_conservative_input_tokens_floors() {
    // The estimate undercounts (tiktoken drift); the observed is the
    // ground truth. The floor takes the max so the gate does not under-trip.
    assert_eq!(conservative_input_tokens(30_000, Some(45_000)), 45_000);
    // The estimate overcounts; the observed is smaller. The estimate
    // stands (never undercount below the estimate either — the estimate
    // is the upper bound of the assembled context).
    assert_eq!(conservative_input_tokens(50_000, Some(40_000)), 50_000);
    // No observed yet (first turn): the estimate stands alone.
    assert_eq!(conservative_input_tokens(20_000, None), 20_000);
    assert_eq!(conservative_input_tokens(20_000, Some(0)), 20_000);
}

#[test]
fn test_resolve_capabilities_overrides_only() {
    // The [1m] suffix is an explicit user opt-in and outranks the
    // provider's negotiated window; the provider stays one source in the
    // chain, not an override.
    let provider_caps = ModelCapabilities {
        streaming: true,
        tools: true,
        vision: true,
        context_window: 200_000,
        max_output_tokens: 8_000,
    };
    let resolved = resolve_capabilities("glm-5.2[1m]", provider_caps, None);
    assert_eq!(resolved.context_window, 1_000_000, "suffix opt-in wins");
    assert!(resolved.streaming);
    assert!(resolved.vision);
    assert_eq!(resolved.max_output_tokens, 8_000);
}

#[test]
fn test_caps_trust_provider_unknown() {
    // An unknown model id carries no signal: the provider's negotiated
    // window stands. A provider that deliberately reports a small window
    // (tests, a constrained deployment) stays authoritative — the
    // model-id default never over-reports past the provider.
    let small = ModelCapabilities {
        streaming: false,
        tools: true,
        vision: false,
        context_window: 200,
        max_output_tokens: 1_000,
    };
    let resolved = resolve_capabilities("stub-test-model", small, None);
    assert_eq!(resolved.context_window, 200, "provider window trusted");
    assert_eq!(resolved.max_output_tokens, 1_000);
}

#[test]
fn test_zero_falls_to_catalog() {
    // When the provider reports 0 (unknown — the OpenAI-compatible
    // gateway omits context-length), the catalog resolves the model.
    let caps = ModelCapabilities {
        context_window: 0,
        max_output_tokens: 8_000,
        ..Default::default()
    };
    let resolved = resolve_capabilities("glm-5.2", caps, None);
    assert_eq!(
        resolved.context_window, 1_000_000,
        "provider 0 => catalog wins"
    );
}

#[test]
fn test_zero_unknown_falls_default() {
    // Provider 0 + no catalog entry => DEFAULT_CONTEXT_WINDOW so the
    // pre-flight gate does not false-fire on an unknown model.
    let caps = ModelCapabilities {
        context_window: 0,
        max_output_tokens: 4_000,
        ..Default::default()
    };
    let resolved = resolve_capabilities("totally-unknown-model", caps, None);
    assert_eq!(
        resolved.context_window, DEFAULT_CONTEXT_WINDOW,
        "unknown + provider 0 => conservative default"
    );
    assert_eq!(resolved.max_output_tokens, 4_000);
}

#[test]
fn test_normalize_growing_lowercase_suffix() {
    // Each dotted capital I grows by a byte when lowered; a suffix offset
    // taken from the lowercase copy would run past the id's end.
    assert_eq!(
        normalize_model_for_api("\u{130}\u{130}\u{130}[1m]"),
        "\u{130}\u{130}\u{130}"
    );
}

#[test]
fn test_normalize_shrinking_lowercase_suffix() {
    // The kelvin sign shrinks by two bytes when lowered; a suffix offset
    // taken back on the id would cut inside the char.
    assert_eq!(normalize_model_for_api("\u{212a}[1m]"), "\u{212a}");
}
