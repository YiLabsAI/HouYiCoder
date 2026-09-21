//! Model-adaptive context window + output-token resolution, and
//! effective-token accounting.
//!
//! The provider reports a static default window via capabilities(); the
//! real window depends on the active model id, resolved in priority
//! order: a [1m] suffix opts into the long-context window (authoritative);
//! a per-family version-aware catalog covers open-weight models whose
//! models-list omits context-length; an error-response learner corrects
//! a stale entry when the provider enforces the real limit; unknown
//! models fall back to a conservative default so context ceiling never
//! bricks.
//!
//! Effective-token accounting normalizes the two provider reporting
//! styles: split-accounting (input_tokens is the uncached remainder;
//! cache read + creation are separate fields that must be added) and
//! subset-accounting (input_tokens already includes cached tokens). The
//! effective input is the inclusive total in both cases, computed from
//! the broken-out fields when non-zero so a misreported inclusive field
//! cannot undercount.

use houyicoder_protocol::frontend::model::ContextWindowSource;
use houyicoder_protocol::llm::{EffortLevel, ModelCapabilities, Usage};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// The byte span in the source of the chars whose lowercase forms begin at
/// or after the start offset and end before the end offset in the lowercase
/// copy. Lowercasing can change a char's byte length in either direction
/// (the dotted capital I grows two to three bytes, the kelvin sign shrinks
/// three to one), so an offset from the copy can land inside a char's form,
/// where the answer is the char after it, or reach a char whose form is
/// shorter, where the answer is that char. ASCII maps byte for byte and is
/// counted without building its form. Callers that find a match in a
/// lowercase copy and cut the original must map the copy's offsets back
/// through here, or the cut lands inside a char.
pub(crate) fn span_for_lowercase_offsets(text: &str, start: usize, end: usize) -> (usize, usize) {
    let mut copy_at = 0;
    let mut hit_start = None;
    for (index, ch) in text.char_indices() {
        if hit_start.is_none() && copy_at >= start {
            hit_start = Some(index);
        }
        if copy_at >= end {
            return (hit_start.unwrap_or(index), index);
        }
        copy_at += if ch.is_ascii() {
            1
        } else {
            ch.to_lowercase().map(char::len_utf8).sum()
        };
    }
    let tail = text.len();
    (hit_start.unwrap_or(tail), tail)
}

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
    /// enable_thinking + thinking_budget.
    Qwen3,
    /// reasoning_effort.
    OpenaiReasoning,
    /// reasoning_effort.
    Glm,
    /// No effort parameter.
    NotSupported,
}

/// Probe the model id for its effort dialect. qwen3 wins over other families
/// (a hypothetical qwen3-reasoning id is qwen3 first). Keep in lockstep with
/// the provider's own probe so the resolved dialect and the emitted fields
/// cannot drift.
pub fn effort_dialect(model: &str) -> EffortDialect {
    let m = model.to_lowercase();
    if m.contains("qwen3") {
        EffortDialect::Qwen3
    } else if m.contains("o1")
        || m.contains("o3")
        || m.contains("gpt-5")
        || m.contains("deepseek-v4")
    {
        EffortDialect::OpenaiReasoning
    } else if m.contains("glm-5.2") || m.contains("glm-5.3") {
        EffortDialect::Glm
    } else {
        EffortDialect::NotSupported
    }
}

/// The effort levels a model's dialect offers. Version-aware only where a
/// family splits its ladder: glm-5.3 dropped medium (low/high/max, glm-5.2
/// keeps low/medium/high/max); deepseek-v4 follows DeepSeek's
/// reasoning_effort set low/high/max. Xhigh stays out of every dialect, so a
/// stale upper pick clamps to the dialect's top rather than sending an
/// unverified value. glm-5 / glm-5.1 predate reasoning-effort and serve no
/// ladder.
pub fn dialect_effort_levels(model: &str) -> &'static [EffortLevel] {
    let m = model.to_lowercase();
    match effort_dialect(model) {
        EffortDialect::Qwen3 => &[EffortLevel::Low, EffortLevel::Medium, EffortLevel::High],
        EffortDialect::OpenaiReasoning => {
            if m.contains("deepseek-v4") {
                &[EffortLevel::Low, EffortLevel::High, EffortLevel::Max]
            } else {
                &[EffortLevel::Low, EffortLevel::Medium, EffortLevel::High]
            }
        }
        EffortDialect::Glm => {
            if m.contains("glm-5.3") {
                &[EffortLevel::Low, EffortLevel::High, EffortLevel::Max]
            } else {
                &[
                    EffortLevel::Low,
                    EffortLevel::Medium,
                    EffortLevel::High,
                    EffortLevel::Max,
                ]
            }
        }
        EffortDialect::NotSupported => &[],
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
    // Strip [1m] and [2m] case-insensitively; the suffix may appear once.
    // The match is found in the copy but the cut is made in the original,
    // whose byte lengths differ when a char's form changes size.
    for suffix in ["[1m]", "[2m]"] {
        if let Some(pos) = lower.find(suffix) {
            let (start, end) = span_for_lowercase_offsets(model, pos, pos + suffix.len());
            return format!("{}{}", &model[..start], &model[end..])
                .trim()
                .to_string();
        }
    }
    model.trim().to_string()
}

/// Best-effort context window for well-known open-weight model families
/// served by OpenAI-compatible gateways whose models-list endpoint omits
/// the context-length field. Keyed on the lowercased model id by substring
/// match, so the canonical spelling of a catalogued family resolves it. The
/// first substring hit wins; the catalog is ordered most-specific first so a
/// broad entry cannot absorb a narrow one. The error-response learner
/// overrides a stale entry earlier in the flow the first time the provider
/// enforces the real limit.
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

/// Resolve the context window the next request is measured against, with its
/// provenance. One chain serves both the pane's display row and the
/// pre-flight gate, so the number the user reads is the number the request
/// uses: learned (provider-enforced limits) first, then the [1m] suffix
/// opt-in, the user's per-model override, the shipped family table, the
/// provider's negotiated window, and the conservative default last.
pub fn resolve_window_info(
    model: &str,
    provider_window: u32,
    config_override: Option<u32>,
) -> (u32, ContextWindowSource) {
    if let Some(learned) = lookup_learned_context_window(model) {
        return (learned, ContextWindowSource::Learned);
    }
    if has_long_context_suffix(model) {
        return (LONG_CONTEXT_WINDOW, ContextWindowSource::ModelSuffix);
    }
    if let Some(window) = config_override {
        return (window, ContextWindowSource::ExplicitConfig);
    }
    if let Some(window) = catalog_window(model) {
        return (window, ContextWindowSource::ModelCatalog);
    }
    if provider_window > 0 {
        return (provider_window, ContextWindowSource::Provider);
    }
    (DEFAULT_CONTEXT_WINDOW, ContextWindowSource::Fallback)
}

/// Resolve a full ModelCapabilities for a model id, on the same chain as
/// resolve_window_info so the display row and the request gate never
/// disagree: a provider that reports a non-zero context window is one source
/// in the chain, not an override — the user's explicit override and a
/// provider-enforced learned limit both rank above it. The other capability
/// flags always come from the provider.
pub fn resolve_capabilities(
    model: &str,
    provider_caps: ModelCapabilities,
    config_override: Option<u32>,
) -> ModelCapabilities {
    let (window, _source) =
        resolve_window_info(model, provider_caps.context_window, config_override);
    ModelCapabilities {
        context_window: window,
        ..provider_caps
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

/// The conservative input-token count for the pre-flight gate: the max of
/// the local tiktoken estimate and the last turn's observed input tokens.
/// The estimate can undercount on non-tiktoken-native models (glm/qwen);
/// the observed is the provider's ground truth. The max never under-trips
/// on an undercount, at the cost of an occasional early compact.
pub fn conservative_input_tokens(estimate: u32, last_observed_input: Option<u64>) -> u32 {
    match last_observed_input {
        Some(obs) if obs > 0 => estimate.max(obs as u32),
        _ => estimate,
    }
}

/// Fill a usage the provider omitted with the locally-estimated input
/// tokens. Some OpenAI-compat streams do not honor
/// stream_options.include_usage, so the finish arrives with usage 0. The
/// estimate (tiktoken over the assembled context) is the same number
/// record_turn receives, so downstream readers see the real footprint.
pub fn fill_omitted_usage(usage: &mut Usage, estimated_input_tokens: u32) {
    if usage.input_tokens == 0 {
        usage.input_tokens = estimated_input_tokens;
        if usage.non_cached_input_tokens == 0 {
            usage.non_cached_input_tokens = estimated_input_tokens;
        }
        if usage.total_tokens == 0 {
            usage.total_tokens = estimated_input_tokens.saturating_add(usage.output_tokens);
        }
    }
}

#[cfg(test)]
#[path = "model_window_tests.rs"]
mod tests;
