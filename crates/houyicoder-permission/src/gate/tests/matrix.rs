use super::*;
use crate::decision::Outcome;
use houyicoder_api::sandbox::SideEffect;

// Mode policy matrix tests: mode_default after the pipeline declines.
// Manual allows only read-only, non-destructive calls; Auto also allows a
// mutating call the tool declares reversible. None = a call the side-effect
// table leaves unclassified; known reads and unknown names share it.
// Mode   | None read-only | None mutating | None destructive | None native | FS/Exec | Network |
// -------|----------------|---------------|------------------|-------------|---------|---------|
// Manual | Allow          | Ask           | Ask              | Ask         | Ask     | Ask     |
// Auto   | Allow          | Allow         | Ask              | Ask         | Allow   | Ask     |

// Manual: None(n=F) -> Allow, None(n=T) -> Ask, FS/Exec/Net -> Ask.

#[test]
fn test_manual_none_allows() {
    let r = req("read", false, true, false);
    assert!(matches!(
        mode_default(PermissionMode::Manual, &r).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_manual_none_native_asks() {
    let r = req("read", false, true, true);
    assert!(matches!(
        mode_default(PermissionMode::Manual, &r).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_manual_fs_asks() {
    let r = req("edit", false, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Manual, &r).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_manual_exec_asks() {
    let r = req("bash", false, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Manual, &r).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_manual_net_asks() {
    let r = req("webfetch", false, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Manual, &r).outcome(),
        Outcome::Ask
    ));
}

// Auto: Exec/FS -> Allow, None(n=F) -> Allow, None(n=T) -> Ask, Net -> Ask.

#[test]
fn test_auto_none_allows() {
    let r = req("read", false, true, false);
    assert!(matches!(
        mode_default(PermissionMode::Auto, &r).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_auto_none_native_asks() {
    let r = req("read", false, true, true);
    assert!(matches!(
        mode_default(PermissionMode::Auto, &r).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_auto_fs_allows() {
    let r = req("edit", false, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Auto, &r).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_auto_exec_allows() {
    let r = req("bash", false, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Auto, &r).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_auto_net_asks() {
    let r = req("webfetch", false, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Auto, &r).outcome(),
        Outcome::Ask
    ));
}

// Direct policy edge cases: verify struct behavior where the side-effect does
// not derive from a shell command (the pipeline catches destructive shell
// content before mode_default runs).

#[test]
fn test_manual_policy_native_asks() {
    // Manual asks a None tool that declares native_requires_approval.
    let r = req("read", false, true, true);
    assert!(matches!(
        DefaultPolicy.decide(&r, SideEffect::None).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_auto_policy_exec_allows() {
    // Auto allows exec at the policy level; the pipeline's destructive gate
    // catches dangerous shell commands before this runs.
    let r = req("bash", true, false, true);
    assert!(matches!(
        AutoPolicy.decide(&r, SideEffect::Exec).outcome(),
        Outcome::Allow
    ));
}

/// A name the effect table does not know is not a pure read merely because
/// the table has no row for it: an external tool that does not declare itself
/// both read-only and non-destructive keeps Manual's question.
#[test]
fn test_manual_none_mutating_asks() {
    let r = req("custom_mut", false, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Manual, &r).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        mode_default(PermissionMode::Auto, &r).outcome(),
        Outcome::Allow
    ));
}

/// The trait defaults answer mutating and destructive, which is the shape a
/// remote adapter has unless it says otherwise: both modes ask before it
/// runs, so no pipeline stage has to have classified its name.
#[test]
fn test_unknown_default_tool_asks() {
    let r = req("mcp_custom", true, false, false);
    assert!(matches!(
        mode_default(PermissionMode::Auto, &r).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        mode_default(PermissionMode::Manual, &r).outcome(),
        Outcome::Ask
    ));
}

/// An unknown name that declares itself read-only keeps the read row: the
/// fallback trusts a positive read-only declaration, only the absence of one
/// escalates.
#[test]
fn test_unknown_read_only_allows() {
    let r = req("mcp_search", false, true, false);
    for mode in [PermissionMode::Manual, PermissionMode::Auto] {
        assert!(matches!(mode_default(mode, &r).outcome(), Outcome::Allow));
    }
}

/// A contradictory declaration asks: a tool may not buy the read row while
/// also reporting its call destructive.
#[test]
fn test_destructive_read_claim_asks() {
    let r = req("odd_claim", true, true, false);
    for mode in [PermissionMode::Manual, PermissionMode::Auto] {
        assert!(matches!(mode_default(mode, &r).outcome(), Outcome::Ask));
    }
}
