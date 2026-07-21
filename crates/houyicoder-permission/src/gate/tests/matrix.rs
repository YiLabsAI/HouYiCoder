use super::*;
use crate::decision::Outcome;
use houyicoder_api::sandbox::SideEffect;

// Mode policy matrix tests — tests mode_default directly. The pipeline
// safety gates (rules, safety, compound, destructive, consent) are tested in
// the mode-decision suite. Two modes: Manual asks before any tool that
// declares it needs approval (read-only auto-allows); Auto allows safe ops
// and only asks for destructive ones.
//
// Matrix (mode_default verdict for requests that pass the pipeline):
// Mode   | None(n=F) | None(n=T) | Filesystem | Exec  | Network |
// -------|-----------|-----------|------------|-------|---------|
// Manual | Allow     | Ask       | Ask        | Ask   | Ask     |
// Auto   | Allow     | Ask       | Allow      | Allow | Ask     |
//
// (n = native_requires_approval; destructive shell commands are caught by
// should_ask_destructive in the pipeline, not by mode_default.)

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
