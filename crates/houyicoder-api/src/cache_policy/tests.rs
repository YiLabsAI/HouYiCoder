//! Tests for this module.

use super::*;

#[test]
fn test_auto_places_three_breakpoints() {
    let policy = AutoCachePolicy;
    let mut breakpoints = Vec::new();
    policy.apply(&mut breakpoints, &policy.policy());
    assert_eq!(breakpoints.len(), 3, "Auto places 3 breakpoints");
    // Order: system static prefix, last tool def, latest user message.
    assert_eq!(breakpoints[0].kind, BreakpointKind::SystemStaticPrefix);
    assert_eq!(breakpoints[1].kind, BreakpointKind::LastToolDef);
    assert_eq!(breakpoints[2].kind, BreakpointKind::LatestUserMessage);
    // All ephemeral 1h.
    assert!(
        breakpoints
            .iter()
            .all(|bp| matches!(bp.hint, CacheHint::Ephemeral(CacheTtl::OneHour)))
    );
}

#[test]
fn test_none_places_no_breakpoints() {
    let policy = NoCachePolicy;
    let mut breakpoints = vec![CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];
    policy.apply(&mut breakpoints, &policy.policy());
    assert!(breakpoints.is_empty(), "None clears breakpoints");
}

#[test]
fn test_apply_clears_before_pushing() {
    // Re-applying Auto does not stack duplicates (the vec clears first).
    let policy = AutoCachePolicy;
    let mut breakpoints = Vec::new();
    policy.apply(&mut breakpoints, &CachePolicy::Auto);
    policy.apply(&mut breakpoints, &CachePolicy::Auto);
    assert_eq!(breakpoints.len(), 3, "re-apply does not stack");
}

#[test]
fn test_policy_reports_mode() {
    assert_eq!(AutoCachePolicy.policy(), CachePolicy::Auto);
    assert_eq!(NoCachePolicy.policy(), CachePolicy::None);
}
