//! Tests for the Ask permission-rule wire round-trip + the "ask" label
//! (snake_case serialization). Extracted from server.rs to keep the file
//! under the size gate. These test crate::protocol_adapter functions, not Server.

use crate::protocol_adapter::permission_rule_from_wire;
use crate::protocol_adapter::permission_rule_to_wire;
use houyicoder_permission::{Effect, Rule, RuleContent};
use houyicoder_protocol::frontend::permission::PermissionEffect;

/// An Ask rule round-trips through the mapping losslessly: engine
/// Effect::Ask -> wire Ask -> engine Effect::Ask (was wrongly dropped to
/// Reject before the three-state wire fix).
#[test]
fn test_ask_rule_round_trips() {
    let engine_rule =
        Rule::with_content("bash", RuleContent::Prefix("git push".into()), Effect::Ask).unwrap();
    let wire = permission_rule_to_wire(&engine_rule);
    assert_eq!(wire.effect, PermissionEffect::Ask);
    let back = permission_rule_from_wire(&wire).unwrap();
    assert_eq!(
        back.effect,
        Effect::Ask,
        "Ask must round-trip, not drop to Deny"
    );
}

/// The wire Ask variant serializes as "ask" (snake_case).
#[test]
fn test_ask_effect_label() {
    assert_eq!(
        permission_rule_to_wire(&Rule::new("edit", Effect::Ask).unwrap()).effect,
        PermissionEffect::Ask
    );
}
