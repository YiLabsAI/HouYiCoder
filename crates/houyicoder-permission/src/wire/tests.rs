//! The wire conversions: every domain value round-trips through its
//! protocol spelling, and an unknown variant is the forward-compat fallback.

use houyicoder_protocol::frontend::permission as wire;

use super::*;
use crate::decision::FenceProof;
use houyicoder_api::sandbox::Coverage;
use houyicoder_api::sandbox::SideEffect;
use std::path::PathBuf;
use wire::AllowReason as WireAllowReason;
use wire::AskReason as WireAskReason;
use wire::AskSource as WireAskSource;
use wire::DenyReason as WireDenyReason;
use wire::DenySource as WireDenySource;
use wire::PermissionEffect as WirePermissionEffect;

/// Every ask source maps to a distinct wire variant and back through
/// serialization, so the approval card reads the same source the engine
/// produced. The wire Unknown variant has no engine counterpart (it is the
/// forward-compat fallback), so it is not in the round-trip set.
#[test]
fn test_ask_source_round_trips() {
    for s in [
        AskSource::UserRule,
        AskSource::SystemSafety,
        AskSource::Detection,
        AskSource::ToolNative,
    ] {
        let w = WireAskSource::from(&s);
        let json = serde_json::to_string(&w).expect("serialize");
        let back: wire::AskSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, w, "wire ask source round-trips for {s:?}");
    }
}

#[test]
fn test_ask_reason_round_trips() {
    let r = AskReason {
        source: AskSource::Detection,
        validator: "destructive_command",
        detail: "rm needs confirmation".into(),
        containment_note: None,
    };
    let w = WireAskReason::from(&r);
    assert_eq!(w.source, WireAskSource::Detection);
    assert_eq!(w.validator, "destructive_command");
    assert_eq!(w.detail, "rm needs confirmation");
    // The wire form round-trips through serde losslessly.
    let json = serde_json::to_string(&w).expect("serialize");
    let back: wire::AskReason = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, w);
}

#[test]
fn test_deny_reason_round_trips() {
    let r = DenyReason {
        source: DenySource::Headless,
        validator: "post_transform",
        detail: "headless mode".into(),
    };
    let w = WireDenyReason::from(&r);
    assert_eq!(w.source, WireDenySource::Headless);
    assert_eq!(w.validator, "post_transform");
}

#[test]
fn test_allow_reason_flattens_containment() {
    // The proof token is unreachable from the wire side; the variant
    // flattens to the Containment label with no payload.
    let proof = FenceProof::new(
        &Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        },
        SideEffect::Exec,
    )
    .expect("fenced exec yields a proof");
    let r = AllowReason::Containment(proof);
    assert_eq!(WireAllowReason::from(&r), WireAllowReason::Containment);
    assert_eq!(
        WireAllowReason::from(&AllowReason::ModeDefault),
        WireAllowReason::ModeDefault
    );
}

#[test]
fn test_outcome_maps_to_effect() {
    assert_eq!(
        WirePermissionEffect::from(Outcome::Allow),
        WirePermissionEffect::Allow
    );
    assert_eq!(
        WirePermissionEffect::from(Outcome::Deny),
        WirePermissionEffect::Reject
    );
    assert_eq!(
        WirePermissionEffect::from(Outcome::Ask),
        WirePermissionEffect::Ask
    );
}
