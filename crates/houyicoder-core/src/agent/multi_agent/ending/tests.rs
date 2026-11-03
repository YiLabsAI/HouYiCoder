//! Tests for this module.

use super::*;

#[test]
fn test_foreground_starts_undecided() {
    let claim = EndingClaim::new(ChildRunMode::Foreground);
    assert!(!claim.notifies_parent());
    assert!(claim.claim_caller(), "the waiting caller takes the ending");
    assert!(!claim.notifies_parent());
}

#[test]
fn test_handover_blocks_caller() {
    let claim = EndingClaim::new(ChildRunMode::Foreground);
    assert!(claim.claim_notification(), "the first hand-over decides");
    assert!(!claim.claim_caller(), "the caller no longer has an ending");
    assert!(claim.notifies_parent());
}

#[test]
fn test_caller_claim_is_idempotent() {
    let claim = EndingClaim::new(ChildRunMode::Foreground);
    assert!(claim.claim_caller());
    assert!(
        claim.claim_caller(),
        "a second report by the same side is not a hand-over"
    );
}

#[test]
fn test_background_starts_notifying() {
    let claim = EndingClaim::new(ChildRunMode::Background);
    assert!(claim.notifies_parent());
    assert!(!claim.claim_caller());
    assert!(
        !claim.claim_notification(),
        "an already backgrounded child has nothing to hand over"
    );
}

#[test]
fn test_second_handover_loses() {
    let claim = EndingClaim::new(ChildRunMode::Foreground);
    assert!(claim.claim_notification());
    assert!(!claim.claim_notification());
}
