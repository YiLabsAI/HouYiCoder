//! Tests for this module.

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
