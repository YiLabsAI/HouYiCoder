//! Tests for this module.

use super::SideEffect;

#[test]
fn test_label_round_trips() {
    assert_eq!(SideEffect::None.label(), "none");
    assert_eq!(SideEffect::Filesystem.label(), "filesystem");
    assert_eq!(SideEffect::Network.label(), "network");
    assert_eq!(SideEffect::Exec.label(), "exec");
}
