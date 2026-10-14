use super::*;
use std::path::PathBuf;

fn allow() -> Decision {
    Decision::Allow(AllowReason::ModeDefault)
}
fn ask() -> Decision {
    Decision::Ask(AskReason {
        source: AskSource::Detection,
        validator: "test",
        detail: "x".into(),
        containment_note: None,
    })
}
fn deny() -> Decision {
    Decision::Deny(DenyReason {
        source: DenySource::Headless,
        validator: "test",
        detail: "x".into(),
    })
}

#[test]
fn test_outcome_maps_variants() {
    assert_eq!(allow().outcome(), Outcome::Allow);
    assert_eq!(ask().outcome(), Outcome::Ask);
    assert_eq!(deny().outcome(), Outcome::Deny);
}

/// A fenced exec yields a proof; an unfenced call or a non-exec side effect
/// does not. Pins the C2 eligibility at the type level.
#[test]
fn test_fence_proof_eligibility() {
    let fenced = Coverage::Fenced {
        writable_roots: vec![PathBuf::from("/ws")],
    };
    assert!(FenceProof::new(&fenced, SideEffect::Exec).is_some());
    assert!(FenceProof::new(&Coverage::Unfenced, SideEffect::Exec).is_none());
    assert!(FenceProof::new(&fenced, SideEffect::Filesystem).is_none());
}
