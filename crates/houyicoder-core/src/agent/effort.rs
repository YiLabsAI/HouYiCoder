//! Effort resolution chain: pick the effort level a request will actually
//! carry, following the layered precedence the picker + catalog + per-model
//! default define. The chain (env override removed; the
//! test-knob-as-authority bug fixed): the in-session pick wins, then the
//! catalog entry's persisted effort, then the global effort_level
//! fallback, then a per-model built-in default, then None (no effort
//! parameter sent).
//!
//! The catalog layers (catalog[id].effort + model.effort_level) live in the
//! config crate, which the agent layer cannot depend on (config is a leaf
//! below core; core stays free of config I/O). So the catalog read crosses
//! the boundary through the ModelCatalogResolver port: the agent loop holds
//! the trait, the composition root supplies an impl backed by the loaded
//! ModelSection. None in the resolver means no catalog is configured (the stub
//! path) and the chain stops at the in-session pick + the built-in default.

use houyicoder_protocol::llm::{EffortLevel, ModelSettings};

use super::model_window::{EffortDialect, dialect_effort_levels, effort_dialect};

/// Thinking budget the qwen3 family sends for Medium effort (the effort-to-params table).
pub const QWEN_THINKING_BUDGET_MEDIUM: u32 = 8_192;
/// Thinking budget the qwen3 family sends for High effort (the effort-to-params table).
pub const QWEN_THINKING_BUDGET_HIGH: u32 = 16_384;

/// Read the catalog-side effort layers (catalog[id].effort →
/// model.effort_level) for a model. The impl lives at the composition root
/// (backed by the loaded ModelSection); the agent loop calls it only when the
/// in-session pick is None. None from the resolver means the catalog has no
/// effort for this model (or no catalog is configured), and the chain falls to the
/// per-model default.
pub trait ModelCatalogResolver: Send + Sync {
    fn catalog_effort(&self, model: &str) -> Option<EffortLevel>;

    /// The catalog's per-model list of effort levels (ModelEntry
    /// effort_levels). None keeps the dialect's full set. A listed level the
    /// dialect does not serve is dropped, so the list cannot grant what the
    /// model cannot speak.
    fn catalog_effort_levels(&self, _model: &str) -> Option<Vec<EffortLevel>> {
        None
    }

    /// The user-set context_window override for a model (ModelEntry.context_window),
    /// above the family-default table. None when the catalog has no override
    /// for this model — the family default + learned limits apply.
    fn catalog_context_window(&self, _model: &str) -> Option<u32> {
        None
    }

    /// The user-set max_output_tokens override for a model
    /// (ModelEntry.max_output_tokens), above the family default. None when the
    /// catalog has no override — the family default applies. The pre-flight
    /// reserve and the request body share this value (same source, no
    /// overflow when the two disagree).
    fn catalog_max_output_tokens(&self, _model: &str) -> Option<u32> {
        None
    }

    /// Whether the catalog declares a Fast tier for a model
    /// (ModelEntry.fast). None when the catalog says nothing, which reads as
    /// unavailable — no name probe grants the tier, because a family name is
    /// no evidence that an endpoint offers a faster variant of it.
    fn catalog_fast(&self, _model: &str) -> Option<bool> {
        None
    }
}

/// The built-in per-model effort default: the level an "auto" pick (no
/// in-session pick, no catalog entry) resolves to. Reasoning dialects get
/// their ladder's middle (Medium where offered, else the middle rung); an
/// unsupported model gets None — no effort parameter, the API applies its
/// own default.
pub fn effort_default_for(model: &str) -> Option<EffortLevel> {
    // The level an "auto" pick resolves to when no per-model effort is pinned
    // (catalog effort is null). Prefer Medium (the recommended middle); for a
    // dialect whose ladder has no Medium, take the ladder's middle level. The
    // result is a level the dialect offers — a catalog filter may still narrow
    // it to the nearest level the catalog allows.
    let levels = dialect_effort_levels(model);
    levels
        .iter()
        .find(|l| **l == EffortLevel::Medium)
        .copied()
        .or_else(|| levels.get(levels.len() / 2).copied())
}

/// The effort levels available for a model: the dialect's set, minus any
/// the catalog's per-model list excludes. A listed level the dialect does
/// not carry is dropped, and an empty result reads as no effort.
pub fn effort_levels_for(
    model: &str,
    resolver: Option<&dyn ModelCatalogResolver>,
) -> Vec<EffortLevel> {
    let dialect_levels = dialect_effort_levels(model).to_vec();
    match resolver.and_then(|r| r.catalog_effort_levels(model)) {
        Some(listed) => dialect_levels
            .into_iter()
            .filter(|level| listed.contains(level))
            .collect(),
        None => dialect_levels,
    }
}

/// Resolve the effort level a request should carry, following the chain:
/// active_effort → catalog (via the resolver) → per-model default → None.
/// Short-circuits to None when no levels are available (no dialect, or a
/// per-model list that emptied the set). A resolved level above the
/// available set clamps to the set's top — a stale pick above what the
/// model accepts asks for a tier it lacks, the same way an unoffered Fast
/// tier clamps down.
pub fn resolve_applied_effort(
    model: &str,
    active: Option<EffortLevel>,
    resolver: Option<&dyn ModelCatalogResolver>,
) -> Option<EffortLevel> {
    let available = effort_levels_for(model, resolver);
    if available.is_empty() {
        return None;
    }
    active
        .or_else(|| resolver.and_then(|r| r.catalog_effort(model)))
        .or_else(|| effort_default_for(model))
        .map(|level| {
            available
                .iter()
                .copied()
                .rev()
                .find(|a| *a <= level)
                .unwrap_or(available[0])
        })
}

/// Fill a ModelSettings with the effort-derived fields the request body
/// emits, by dialect + resolved effort level (the effort-to-params table): qwen3 gets
/// enable_thinking + thinking_budget (Low turns thinking off and ships no
/// budget — a contradictory request); the reasoning branch gets
/// reasoning_effort; an unsupported model gets nothing. The caller fills
/// max_output_tokens separately (it shares a source with the pre-flight
/// reservation, the shared-source task). Leaves any caller-set field untouched when the
/// effort level is None (auto) so a caller's explicit override is not
/// clobbered.
pub fn apply_effort_settings(
    settings: &mut ModelSettings,
    model: &str,
    effort: Option<EffortLevel>,
) {
    match effort_dialect(model) {
        EffortDialect::Qwen3 => match effort {
            Some(EffortLevel::Low) => settings.enable_thinking = Some(false),
            Some(EffortLevel::Medium) => {
                settings.enable_thinking = Some(true);
                settings.thinking_budget = Some(QWEN_THINKING_BUDGET_MEDIUM);
            }
            Some(EffortLevel::High) => {
                settings.enable_thinking = Some(true);
                settings.thinking_budget = Some(QWEN_THINKING_BUDGET_HIGH);
            }
            // The resolution clamp keeps the upper rungs off the qwen
            // ladder until their budgets are verified.
            _ => {}
        },
        EffortDialect::OpenaiReasoning | EffortDialect::Glm => {
            if let Some(e) = effort {
                settings.reasoning_effort = Some(e);
            }
        }
        EffortDialect::NotSupported => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolver with a fixed catalog effort, for chain-order tests.
    struct FixedCatalog(Option<EffortLevel>);
    impl ModelCatalogResolver for FixedCatalog {
        fn catalog_effort(&self, _model: &str) -> Option<EffortLevel> {
            self.0
        }
    }

    /// A resolver that lists per-model levels, for the exclude rules.
    struct ListedCatalog(Vec<EffortLevel>);
    impl ModelCatalogResolver for ListedCatalog {
        fn catalog_effort(&self, _model: &str) -> Option<EffortLevel> {
            None
        }
        fn catalog_effort_levels(&self, _model: &str) -> Option<Vec<EffortLevel>> {
            Some(self.0.clone())
        }
    }

    /// The OpenAI reasoning dialect offers only the three levels with verified
    /// wire values; xhigh and max stay out until the API spec confirms them,
    /// so a stale upper pick clamps to High before it reaches the wire.
    #[test]
    fn test_reasoning_clamps_upper_pick() {
        assert_eq!(
            resolve_applied_effort("gpt-5.6", Some(EffortLevel::XHigh), None),
            Some(EffortLevel::High)
        );
        let resolved = resolve_applied_effort("gpt-5.6", Some(EffortLevel::Max), None);
        assert_eq!(resolved, Some(EffortLevel::High));
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "gpt-5.6", resolved);
        assert_eq!(
            s.reasoning_effort,
            Some(EffortLevel::High),
            "the clamped level lowers to the verified wire value"
        );
    }

    /// The qwen3 budget ladder stops at High: an XHigh pick clamps to the
    /// top of the available set rather than sending an unverified budget.
    #[test]
    fn test_qwen_clamps_upper_pick() {
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", Some(EffortLevel::XHigh), None),
            Some(EffortLevel::High)
        );
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "qwen3.7-max", Some(EffortLevel::Max));
        assert!(
            s.thinking_budget.is_none(),
            "an unverified budget never reaches the wire"
        );
    }

    /// A per-model list removes levels from the dialect's set, and cannot
    /// add what the dialect does not carry.
    #[test]
    fn test_per_model_list_excludes() {
        let r = ListedCatalog(vec![EffortLevel::Low, EffortLevel::High]);
        assert_eq!(
            effort_levels_for("qwen3.7-max", Some(&r)),
            vec![EffortLevel::Low, EffortLevel::High]
        );
        // A listed upper rung the qwen ladder does not verify is dropped.
        let grants = ListedCatalog(vec![EffortLevel::High, EffortLevel::XHigh]);
        assert_eq!(
            effort_levels_for("qwen3.7-max", Some(&grants)),
            vec![EffortLevel::High],
            "the list cannot grant what the dialect does not carry"
        );
        // A pick above the remaining set clamps to its top.
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", Some(EffortLevel::Max), Some(&r)),
            Some(EffortLevel::High)
        );
    }

    #[test]
    fn test_active_pick_wins_catalog() {
        // The in-session pick is authoritative; a catalog entry does not
        // shadow it.
        let r = FixedCatalog(Some(EffortLevel::Low));
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", Some(EffortLevel::High), Some(&r)),
            Some(EffortLevel::High)
        );
    }

    #[test]
    fn test_catalog_wins_over_default() {
        // No active pick: the catalog entry stands above the built-in default.
        let r = FixedCatalog(Some(EffortLevel::Medium));
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", None, Some(&r)),
            Some(EffortLevel::Medium)
        );
    }

    #[test]
    fn test_auto_falls_to_default() {
        // No active pick, no catalog entry: the chain falls to the built-in
        // default, which for a reasoning dialect is the ladder's middle
        // level (Medium for qwen3) — auto no longer means "no reasoning".
        let r = FixedCatalog(None);
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", None, Some(&r)),
            Some(EffortLevel::Medium)
        );
    }

    #[test]
    fn test_no_resolver_falls_default() {
        // No catalog configured: an active pick is returned; auto falls to the
        // built-in default (Medium for qwen3).
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", Some(EffortLevel::Low), None),
            Some(EffortLevel::Low)
        );
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", None, None),
            Some(EffortLevel::Medium)
        );
    }

    #[test]
    fn test_unsupported_dialect_none() {
        // the unsupported-dialect invariant: a model the dialect probe does not recognize gets no effort,
        // even with an active pick + a catalog entry. The dialect gate is
        // above the chain.
        let r = FixedCatalog(Some(EffortLevel::High));
        assert_eq!(
            resolve_applied_effort("deepseek-chat", Some(EffortLevel::High), Some(&r)),
            None,
            "unsupported model sends no effort regardless of pick/catalog"
        );
    }

    #[test]
    fn test_resolver_send_sync() {
        // The trait object crosses into the runner (Send + Sync); compile-check.
        fn _assert_send_sync<T: ?Sized + Send + Sync>() {}
        _assert_send_sync::<dyn ModelCatalogResolver>();
    }

    #[test]
    fn test_trait_default_catalog_overrides() {
        // A resolver that does not override the catalog_context_window /
        // max_output_tokens defaults yields None for both (the stub path +
        // any minimal impl).
        let r = FixedCatalog(None);
        assert_eq!(r.catalog_context_window("qwen3.7-max"), None);
        assert_eq!(r.catalog_max_output_tokens("qwen3.7-max"), None);
    }

    #[test]
    fn test_apply_qwen_medium() {
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "qwen3.7-max", Some(EffortLevel::Medium));
        assert_eq!(s.enable_thinking, Some(true));
        assert_eq!(s.thinking_budget, Some(QWEN_THINKING_BUDGET_MEDIUM));
        assert!(s.reasoning_effort.is_none());
    }

    #[test]
    fn test_apply_qwen_high() {
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "qwen3-coder", Some(EffortLevel::High));
        assert_eq!(s.enable_thinking, Some(true));
        assert_eq!(s.thinking_budget, Some(QWEN_THINKING_BUDGET_HIGH));
    }

    #[test]
    fn test_apply_qwen_low() {
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "qwen3.7-max", Some(EffortLevel::Low));
        assert_eq!(s.enable_thinking, Some(false));
        assert!(
            s.thinking_budget.is_none(),
            "Low ships no budget — a contradictory request"
        );
    }

    #[test]
    fn test_apply_qwen_none() {
        // Auto (None) does not clobber a caller's explicit fields, and sets
        // nothing on its own.
        let mut s = ModelSettings {
            thinking_budget: Some(4096),
            ..Default::default()
        };
        apply_effort_settings(&mut s, "qwen3.7-max", None);
        assert_eq!(s.thinking_budget, Some(4096), "caller field not clobbered");
        assert!(s.enable_thinking.is_none());
    }

    #[test]
    fn test_apply_reasoning_fills_string() {
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "gpt-5.6", Some(EffortLevel::Low));
        assert_eq!(s.reasoning_effort, Some(EffortLevel::Low));
        assert!(s.enable_thinking.is_none());
        assert!(s.thinking_budget.is_none());
    }

    #[test]
    fn test_apply_unsupported_fills_nothing() {
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "deepseek-chat", Some(EffortLevel::High));
        assert!(s.reasoning_effort.is_none());
        assert!(s.enable_thinking.is_none());
        assert!(s.thinking_budget.is_none());
    }

    /// Auto effort (no pick, no catalog entry) resolves to the dialect's
    /// middle level so a reasoning model thinks under auto — deepseek-v4 has
    /// no Medium, so its middle is High. End-to-end verified: a complex query
    /// produces a ThoughtFor row. Locks the chain (default → resolve → apply
    /// → reasoning_effort) against regressing the default back to None.
    #[test]
    fn test_deepseek_auto_reasons() {
        assert_eq!(
            resolve_applied_effort("deepseek-v4-pro-0813", None, None),
            Some(EffortLevel::High),
            "auto deepseek resolves to High (the ladder middle)"
        );
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "deepseek-v4-pro-0813", Some(EffortLevel::High));
        assert_eq!(
            s.reasoning_effort,
            Some(EffortLevel::High),
            "auto deepseek sends reasoning_effort=high"
        );
        assert!(
            s.enable_thinking.is_none(),
            "deepseek uses reasoning_effort, not enable_thinking"
        );
    }

    /// deepseek-v4 resolves through the reasoning branch: a pick emits
    /// reasoning_effort (max included — DeepSeek's ladder is low/high/max),
    /// never the qwen thinking fields.
    #[test]
    fn test_deepseek_reasoning_effort() {
        assert_eq!(
            resolve_applied_effort("deepseek-v4-pro", Some(EffortLevel::High), None),
            Some(EffortLevel::High)
        );
        assert_eq!(
            resolve_applied_effort("deepseek-v4-pro", Some(EffortLevel::Max), None),
            Some(EffortLevel::Max)
        );
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "deepseek-v4-pro", Some(EffortLevel::Max));
        assert_eq!(s.reasoning_effort, Some(EffortLevel::Max));
        assert!(s.enable_thinking.is_none());
        assert!(s.thinking_budget.is_none());
    }
}
