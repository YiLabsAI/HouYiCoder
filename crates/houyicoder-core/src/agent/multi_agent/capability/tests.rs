//! Tests for this module.

use super::*;

#[test]
fn test_inherits_parent_undeclared() {
    let t = CapabilityToken::effective(PermissionMode::Auto, None);
    assert_eq!(t.mode(), PermissionMode::Auto);
    let t = CapabilityToken::effective(PermissionMode::Manual, None);
    assert_eq!(t.mode(), PermissionMode::Manual);
}

#[test]
fn test_narrowed_parent_manual() {
    let t = CapabilityToken::effective(PermissionMode::Manual, Some(PermissionMode::Auto));
    assert_eq!(
        t.mode(),
        PermissionMode::Manual,
        "parent Manual narrows child"
    );
}

#[test]
fn test_auto_parent_auto_child() {
    let t = CapabilityToken::effective(PermissionMode::Auto, Some(PermissionMode::Auto));
    assert_eq!(t.mode(), PermissionMode::Auto);
}

#[test]
fn test_manual_both() {
    let t = CapabilityToken::effective(PermissionMode::Manual, Some(PermissionMode::Manual));
    assert_eq!(t.mode(), PermissionMode::Manual);
}

/// An agent definition that asks for a mode the parent does not
/// have is downgraded, not amplified. The child runs with the
/// parent's narrower mode, not the agent's wider one.
#[test]
fn test_never_amplifies() {
    let t = CapabilityToken::effective(PermissionMode::Manual, Some(PermissionMode::Auto));
    assert_eq!(t.mode(), PermissionMode::Manual);
}

/// Monotonic: if the parent is already the narrowest, the child
/// cannot be wider regardless of what the agent declares.
#[test]
fn test_monotonic_narrowing() {
    for declared in [PermissionMode::Manual, PermissionMode::Auto] {
        let t = CapabilityToken::effective(PermissionMode::Manual, Some(declared));
        assert_eq!(
            t.mode(),
            PermissionMode::Manual,
            "parent Manual, declared {declared:?}: child must be Manual"
        );
    }
}
