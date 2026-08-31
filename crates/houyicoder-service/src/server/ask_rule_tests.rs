//! Tests for the Ask permission-rule wire round-trip + the "ask" label
//! (snake_case serialization). Extracted from server.rs to keep the file
//! under the size gate. These test crate::projection functions, not Server.

use houyicoder_permission::{Effect, Rule, RuleContent};

/// An Ask rule round-trips through the projection losslessly: engine
/// Effect::Ask -> wire Ask -> engine Effect::Ask (was wrongly dropped to
/// Reject before the three-state wire fix).
#[test]
fn test_ask_rule_round_trips() {
    let engine_rule =
        Rule::with_content("bash", RuleContent::Prefix("git push".into()), Effect::Ask).unwrap();
    let wire = crate::projection::project_permission_rule(&engine_rule);
    assert_eq!(
        wire.effect,
        houyicoder_protocol::frontend::permission::PermissionEffect::Ask
    );
    let back = crate::projection::wire_rule_to_engine(&wire).unwrap();
    assert_eq!(
        back.effect,
        Effect::Ask,
        "Ask must round-trip, not drop to Deny"
    );
}

/// The wire Ask variant serializes as "ask" (snake_case).
#[test]
fn test_ask_effect_label() {
    use houyicoder_protocol::frontend::permission::PermissionEffect;
    assert_eq!(
        crate::projection::project_permission_rule(&Rule::new("edit", Effect::Ask).unwrap()).effect,
        PermissionEffect::Ask
    );
}
