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
//! the boundary through the EffortResolver port: the agent loop holds the
//! trait, the composition root supplies an impl backed by the loaded
//! ModelSection. None in the resolver means no catalog is wired (the stub
//! path) and the chain stops at the in-session pick + the built-in default.

use houyicoder_protocol::llm::{EffortLevel, ModelSettings};

use super::model_window::{EffortDialect, effort_dialect};

/// Thinking budget the qwen3 family sends for Medium effort (the effort-to-params table).
pub const QWEN_THINKING_BUDGET_MEDIUM: u32 = 8_192;
/// Thinking budget the qwen3 family sends for High effort (the effort-to-params table).
pub const QWEN_THINKING_BUDGET_HIGH: u32 = 16_384;

/// Read the catalog-side effort layers (catalog[id].effort →
/// model.effort_level) for a model. The impl lives at the composition root
/// (backed by the loaded ModelSection); the agent loop calls it only when the
/// in-session pick is None. None from the resolver means the catalog has no
/// effort for this model (or no catalog is wired), and the chain falls to the
/// per-model default.
pub trait EffortResolver: Send + Sync {
    fn catalog_effort(&self, model: &str) -> Option<EffortLevel>;

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
}

/// The built-in per-model effort default. The fallback is undefined for
/// unlisted models — no effort parameter is sent, the API applies its own
/// default. No per-model overrides ship yet; the slot is here so a future
/// row lands without reshaping the chain. Returns None for every model
/// today.
pub fn effort_default_for(_model: &str) -> Option<EffortLevel> {
    None
}

/// Resolve the effort level a request should carry, following the chain:
/// active_effort → catalog (via the resolver) → per-model default → None.
/// Short-circuits to None when the model speaks no effort dialect (the unsupported-dialect invariant): a
/// model the dialect probe does not recognize gets no effort parameter even
/// if a stale in-session pick or catalog entry exists.
pub fn resolve_applied_effort(
    model: &str,
    active: Option<EffortLevel>,
    resolver: Option<&dyn EffortResolver>,
) -> Option<EffortLevel> {
    if effort_dialect(model) == EffortDialect::NotSupported {
        return None;
    }
    active
        .or_else(|| resolver.and_then(|r| r.catalog_effort(model)))
        .or_else(|| effort_default_for(model))
}

/// Fill a ModelSettings with the effort-derived fields the request body
/// emits, by dialect + resolved effort level (the effort-to-params table): qwen3 gets
/// enable_thinking + thinking_budget (Low turns thinking off and ships no
/// budget — a contradictory request); the OpenAI reasoning arm gets
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
            None => {}
        },
        EffortDialect::OpenaiReasoning => {
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
    impl EffortResolver for FixedCatalog {
        fn catalog_effort(&self, _model: &str) -> Option<EffortLevel> {
            self.0
        }
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
    fn test_falls_to_none() {
        // No active, no catalog, no default (effort_default_for is None
        // today): the chain yields None — no effort parameter sent.
        let r = FixedCatalog(None);
        assert_eq!(resolve_applied_effort("qwen3.7-max", None, Some(&r)), None);
    }

    #[test]
    fn test_no_resolver_falls_default() {
        // Stub path (no catalog wired): active pick → built-in default → None.
        assert_eq!(
            resolve_applied_effort("qwen3.7-max", Some(EffortLevel::Low), None),
            Some(EffortLevel::Low)
        );
        assert_eq!(resolve_applied_effort("qwen3.7-max", None, None), None);
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
        _assert_send_sync::<dyn EffortResolver>();
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
        apply_effort_settings(&mut s, "o3-mini", Some(EffortLevel::Low));
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
}
