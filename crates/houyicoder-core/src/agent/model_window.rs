//! Model-adaptive context window + output-token resolution, and
//! effective-token accounting.
//!
//! The provider reports a static default window via capabilities(), but the
//! real window depends on the active model id: a [1m] suffix opts into a
//! long-context window (an explicit client-side opt-in, authoritative over
//! all detection); a per-family version-aware catalog covers open-weight
//! models whose models-list endpoint omits the context-length field (Z.AI
//! GLM: 5.2 = 1M, 5/5.1/4.6/4.7 = 200K, 4.5 and earlier = 128K; qwen3 /
//! deepseek / openai-reasoning families publish both a context window and an
//! output-token cap); an error-response learner corrects a stale or wrong
//! catalog entry the first time the provider enforces the real limit in a
//! context-length-exceeded error body; an unknown model falls back to a
//! conservative default so the context-ceiling-never-brick invariant holds.
//!
//! Effective-token accounting normalizes the two provider reporting styles:
//! split-accounting (input_tokens is the uncached remainder; cache read +
//! cache creation are separate fields that must be added) and
//! subset-accounting (input_tokens already includes cached tokens; adding
//! cache again would double-count). The effective input is the inclusive
//! total in both cases, computed from the broken-out fields when they are
//! non-zero so a misreported inclusive field cannot silently undercount.

use houyicoder_protocol::llm::{ModelCapabilities, Usage};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// The conservative default context window for an unknown model. The
/// context-ceiling-never-brick invariant: an unknown model never reports a
/// window larger than this, so the pre-flight gate always errs toward
/// compacting, never toward overflowing.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 200_000;

/// Default output-token cap for a model whose family the catalog does not
/// recognize. Named so every construction site references one place; the
/// TUI inherits it via RunnerConfig::default. A coding agent routinely emits
/// long multi-file replies, so a smaller cap cuts the model mid-sentence
/// (finish_reason length, treated as a natural stop). A known family
/// (qwen3/deepseek/openai-reasoning) overrides this with its published cap;
/// raise the catalog entry when a model supports more.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32_768;

/// Which effort dialect a model speaks, picked by a substring probe on the
/// model id. This is a dialect probe, not a validity check: a typo like
/// qwen3.8-max still matches qwen3, and a non-matching id still runs without
/// effort. NotSupported only drives the effort row's not-supported copy +
/// short-circuits the effort resolution chain (I8); it never adds a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortDialect {
    /// qwen3 family: enable_thinking + thinking_budget.
    Qwen3,
    /// o1/o3/gpt-5 family: reasoning_effort.
    OpenaiReasoning,
    /// Neither family matched: no effort parameters sent.
    NotSupported,
}

/// Probe the model id for its effort dialect. qwen3 wins over the OpenAI
/// reasoning family (a hypothetical qwen3-reasoning id is qwen3 first). The
/// OpenAI reasoning arm matches o1, o3, and gpt-5 substrings case-insensitive.
/// Matches the provider's own probe so the agent loop (which cannot depend on
/// the provider crate) and the request body agree on a model's dialect.
pub fn effort_dialect(model: &str) -> EffortDialect {
    let m = model.to_lowercase();
    if m.contains("qwen3") {
        EffortDialect::Qwen3
    } else if m.contains("o1") || m.contains("o3") || m.contains("gpt-5") {
        EffortDialect::OpenaiReasoning
    } else {
        EffortDialect::NotSupported
    }
}

/// The long-context window a [1m] suffix signals. The suffix is an explicit
/// client-side opt-in, authoritative over capability detection + beta
/// headers.
pub const LONG_CONTEXT_WINDOW: u32 = 1_000_000;

/// GLM-5.2: the first GLM with a truly usable 1M-token context window.
pub const GLM_5_2_CONTEXT_WINDOW: u32 = 1_000_000;

/// GLM-5 / GLM-5.1: 200K context window.
pub const GLM_5_CONTEXT_WINDOW: u32 = 200_000;

/// A published context window for a model family served by an
/// OpenAI-compatible gateway whose models-list endpoint omits the
/// context-length field. The catalog is matched by substring, longest
/// pattern first, so a more specific entry (glm-5.2) wins over a broader
/// one (glm-5). Add a row to the data file to support a new model; the
/// matcher needs no change. The catalog ships as a JSON data file
/// (include_str, compile-time embedded) so the window table is separable
/// from the resolution logic — the same data/logic split the mature
/// catalog tables use — and the hot path stays synchronous and I/O-free
/// (parsed once into a static, never re-read) so the
/// context-ceiling-never-brick invariant never waits on a file read.
#[derive(serde::Deserialize)]
struct CatalogEntry {
    pattern: String,
    window: u32,
    #[serde(default)]
    max_output_tokens: Option<u32>,
}

/// The open-weight context-window catalog, parsed once from the embedded
/// JSON data file. Order matters: longer or more-specific patterns come
/// first so the first substring hit is the most specific. GLM-4 and
/// earlier are intentionally absent — they fall to the 200K default
/// (safe; the error-response learner corrects an over-estimate the first
/// time the provider enforces the real limit).
static MODEL_WINDOWS_JSON: &str = include_str!("../../model_windows.json");

static OPEN_WEIGHT_CATALOG: OnceLock<Vec<CatalogEntry>> = OnceLock::new();

fn open_weight_catalog() -> &'static [CatalogEntry] {
    OPEN_WEIGHT_CATALOG.get_or_init(|| serde_json::from_str(MODEL_WINDOWS_JSON).unwrap_or_default())
}

/// True when the model id carries the long-context opt-in suffix
/// (case-insensitive, may appear anywhere in the id).
pub fn has_long_context_suffix(model: &str) -> bool {
    model.to_lowercase().contains("[1m]")
}

/// Strip the long-context suffixes ([1m] / [2m]) before sending the model id
/// to the provider — the suffix is a client-side opt-in the provider does not
/// recognize and would reject unstripped.
pub fn normalize_model_for_api(model: &str) -> String {
    let lower = model.to_lowercase();
    let mut out = model.to_string();
    // Strip [1m] and [2m] case-insensitively; the suffix may appear once.
    for suffix in ["[1m]", "[2m]"] {
        if let Some(idx) = lower.find(suffix) {
            out = format!("{}{}", &out[..idx], &out[idx + suffix.len()..]);
            break;
        }
    }
    out.trim().to_string()
}

/// Best-effort context window for well-known open-weight model families
/// served by OpenAI-compatible gateways whose models-list endpoint omits
/// the context-length field. Keyed on the lowercased model id so the same
/// family resolves regardless of how the gateway spells version numbers
/// (glm-4.7, glm-47, glm-4p7). The first substring hit wins; the catalog
/// is ordered most-specific first so a broad arm cannot absorb a narrow
/// one. The error-response learner overrides a stale entry earlier in the
/// flow the first time the provider enforces the real limit.
fn open_weight_family_window(model: &str) -> Option<u32> {
    let m = model.to_lowercase();
    open_weight_catalog()
        .iter()
        .find(|entry| m.contains(entry.pattern.as_str()))
        .map(|entry| entry.window)
}

/// Per-family catalog: the window for a model whose id carries a known
/// open-weight family signal. None when the catalog has no entry (the caller
/// falls back to the default). Kept small and explicit so a wrong entry is
/// auditable; the error-response learner can correct a stale value at runtime.
fn catalog_window(model: &str) -> Option<u32> {
    open_weight_family_window(model)
}

/// Per-model learned context windows, recorded from a provider's enforced
/// limit in a context-overflow error body. Keyed on the normalized model id
/// (suffix-stripped, lowercased) so the same model resolves regardless of
/// opt-in suffix. In-memory: cleared on restart, relearned on the first
/// overflow after restart — a learned value is the provider's enforced truth
/// so it overrides the [1m] opt-in and the static catalog alike.
static LEARNED_WINDOWS: OnceLock<RwLock<HashMap<String, u32>>> = OnceLock::new();

fn learned_store() -> &'static RwLock<HashMap<String, u32>> {
    LEARNED_WINDOWS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Record a provider-enforced context limit for a model. Called from the
/// agent loop when a provider's context-overflow error body named the real
/// limit (carried on the ContextOverflow error variant). The enforced value
/// is ground truth, so it overwrites any prior learned value and overrides
/// the static catalog on the next resolution. A no-op when the limit is
/// None (the body carried no parseable number).
pub fn record_learned_context_window(model: &str, enforced_limit: Option<u32>) {
    let Some(limit) = enforced_limit else {
        return;
    };
    let key = normalize_model_for_api(model).to_lowercase();
    if let Ok(mut map) = learned_store().write() {
        map.insert(key, limit);
    }
}

fn lookup_learned_context_window(model: &str) -> Option<u32> {
    let key = normalize_model_for_api(model).to_lowercase();
    let map = learned_store().read().ok()?;
    map.get(&key).copied()
}

/// Resolve the context window for a model id when the id carries a signal.
/// Priority: a learned enforced limit (provider's ground truth) > the [1m]
/// opt-in suffix > the per-family catalog. None for an unknown model with
/// no learned value — the caller then trusts the provider's negotiated
/// window (which in production is the same conservative default). A learned
/// value wins over the [1m] opt-in because a provider that will not serve
/// 1M cannot be opted into it.
pub fn resolve_context_window_opt(model: &str) -> Option<u32> {
    if let Some(learned) = lookup_learned_context_window(model) {
        return Some(learned);
    }
    if has_long_context_suffix(model) {
        return Some(LONG_CONTEXT_WINDOW);
    }
    catalog_window(model)
}

/// Resolve the context window for a model id. Priority: the [1m] suffix
/// (explicit opt-in, authoritative); the per-provider catalog (non-suffix
/// long-window models); the conservative default (unknown models, never
/// over-report). Resolution order: suffix first, then capability/catalog,
/// then the conservative default.
pub fn resolve_context_window(model: &str) -> u32 {
    resolve_context_window_opt(model).unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

/// Resolve a full ModelCapabilities for a model id. Priority: a provider that
/// reports a non-zero context window is authoritative (it negotiated the real
/// per-model limit). When the provider does not know (0 — the common case for
/// OpenAI-compatible gateways that omit context-length), the catalog (family
/// table) + [1m] suffix + learned limits resolve it. An unknown model with no
/// catalog entry falls to the conservative default so the pre-flight gate
/// never false-fires. The other capability flags always come from the provider.
pub fn resolve_capabilities(model: &str, provider_caps: ModelCapabilities) -> ModelCapabilities {
    if provider_caps.context_window > 0 {
        return provider_caps;
    }
    match resolve_context_window_opt(model) {
        Some(w) => ModelCapabilities {
            context_window: w,
            ..provider_caps
        },
        None => ModelCapabilities {
            context_window: DEFAULT_CONTEXT_WINDOW,
            ..provider_caps
        },
    }
}

/// Best-effort output-token cap for a model whose family the catalog
/// recognizes (qwen3 / deepseek / openai-reasoning), matched by the same
/// longest-pattern-first substring rule as the context-window catalog. None
/// when the catalog has no entry for the family (the caller falls back to the
/// default). The catalog override (ModelEntry.max_output_tokens) lands above
/// this layer in a later task; here the family default is the top of the
/// resolution chain.
fn open_weight_family_max_output(model: &str) -> Option<u32> {
    let m = model.to_lowercase();
    open_weight_catalog()
        .iter()
        .find(|entry| m.contains(entry.pattern.as_str()))
        .and_then(|entry| entry.max_output_tokens)
}

/// Resolve the output-token cap for a model id. Chain: the family default
/// (catalog entry) → DEFAULT_MAX_OUTPUT_TOKENS. Reads no process env so a
/// stray env var cannot shadow a persisted pick. The catalog override
/// (ModelEntry.max_output_tokens, the user-set per-model value) is plumbed
/// above this layer in a later task.
pub fn resolve_max_output_tokens(model: &str) -> u32 {
    open_weight_family_max_output(model).unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

/// The effective input-token count for a turn's usage, normalized across
/// split-accounting and subset-accounting providers. When the cache fields
/// are broken out (Anthropic split), the effective input is the uncached
/// remainder plus cache read plus cache creation — the inclusive total the
/// model saw. When they are zero (OpenAI subset, cache already folded into
/// input_tokens), the effective input is input_tokens verbatim. Computing
/// from the broken-out fields when present prevents a misreported inclusive
/// field from silently undercounting.
pub fn effective_input_tokens(usage: &Usage) -> u32 {
    let split = usage.non_cached_input_tokens
        + usage.cache_read_input_tokens
        + usage.cache_write_input_tokens;
    if split > 0 { split } else { usage.input_tokens }
}

/// The effective served-token count for the pre-flight gate: the max of the
/// local tiktoken estimate and the last turn's observed input tokens. The
/// estimate can undercount on non-tiktoken-native models (glm/qwen); the
/// observed is the provider's ground truth for the prefix. The max is a
/// conservative floor — the gate never under-trips on an undercount, at the
/// cost of an occasional early compact when the estimate overcounts.
pub fn effective_served_tokens(estimate: u32, last_observed_input: Option<u64>) -> u32 {
    match last_observed_input {
        Some(obs) if obs > 0 => estimate.max(obs as u32),
        _ => estimate,
    }
}

#[cfg(test)]
mod tests {
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
    fn test_glm4_falls_to_default() {
        // GLM-4 and earlier are intentionally absent from the catalog; they
        // fall to the 200K default rather than over-reporting a window the
        // provider will not serve. The error-response learner corrects an
        // over-estimate the first time the provider enforces the real limit.
        assert_eq!(resolve_context_window("glm-4.6"), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(resolve_context_window("glm-4.5"), DEFAULT_CONTEXT_WINDOW);
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
    fn test_openai_reasoning_family() {
        // o1/o3 entries removed from the catalog (simplified). These models
        // are not served by DashScope; an unknown family falls to the default.
        assert_eq!(resolve_context_window("o1-mini"), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(resolve_context_window("o3"), DEFAULT_CONTEXT_WINDOW);
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
    fn test_served_tokens_floor_estimate() {
        // The estimate undercounts (tiktoken drift); the observed is the
        // ground truth. The floor takes the max so the gate does not under-trip.
        assert_eq!(effective_served_tokens(30_000, Some(45_000)), 45_000);
        // The estimate overcounts; the observed is smaller. The estimate
        // stands (never undercount below the estimate either — the estimate
        // is the upper bound of the served view).
        assert_eq!(effective_served_tokens(50_000, Some(40_000)), 50_000);
        // No observed yet (first turn): the estimate stands alone.
        assert_eq!(effective_served_tokens(20_000, None), 20_000);
        assert_eq!(effective_served_tokens(20_000, Some(0)), 20_000);
    }

    #[test]
    fn test_resolve_capabilities_overrides_only() {
        // The provider's non-zero window is authoritative; the [1m] suffix
        // and catalog are fallbacks for when the provider does not know (0).
        // Here the provider reports 200K — that wins over the catalog.
        let provider_caps = ModelCapabilities {
            streaming: true,
            tools: true,
            vision: true,
            context_window: 200_000,
            max_output_tokens: 8_000,
        };
        let resolved = resolve_capabilities("glm-5.2[1m]", provider_caps);
        assert_eq!(resolved.context_window, 200_000, "provider non-zero wins");
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
        let resolved = resolve_capabilities("stub-test-model", small);
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
        let resolved = resolve_capabilities("glm-5.2", caps);
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
        let resolved = resolve_capabilities("totally-unknown-model", caps);
        assert_eq!(
            resolved.context_window, DEFAULT_CONTEXT_WINDOW,
            "unknown + provider 0 => conservative default"
        );
        assert_eq!(resolved.max_output_tokens, 4_000);
    }
}
