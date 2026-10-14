//! Tests for this module.

use super::*;

#[test]
fn test_mode_round_trips() {
    for mode in [PermissionMode::Manual, PermissionMode::Auto] {
        let json = serde_json::to_string(&mode).expect("serialize");
        let back: PermissionMode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, mode);
    }
}

#[test]
fn test_mode_snake_case_tags() {
    let json = serde_json::to_string(&PermissionMode::Manual).expect("serialize");
    assert_eq!(json, "\"manual\"");
    let json = serde_json::to_string(&PermissionMode::Auto).expect("serialize");
    assert_eq!(json, "\"auto\"");
}

#[test]
fn test_effect_round_trips() {
    for e in [
        PermissionEffect::Allow,
        PermissionEffect::Reject,
        PermissionEffect::Ask,
    ] {
        let json = serde_json::to_string(&e).expect("serialize");
        let back: PermissionEffect = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, e);
    }
    assert_eq!(
        serde_json::to_string(&PermissionEffect::Allow).unwrap(),
        "\"allow\""
    );
    assert_eq!(
        serde_json::to_string(&PermissionEffect::Reject).unwrap(),
        "\"reject\""
    );
    assert_eq!(
        serde_json::to_string(&PermissionEffect::Ask).unwrap(),
        "\"ask\""
    );
}

#[test]
fn test_content_round_trips_tier() {
    let exact = PermissionRuleContent::Exact {
        value: "npm install".into(),
    };
    let prefix = PermissionRuleContent::Prefix {
        value: "npm".into(),
    };
    let glob = PermissionRuleContent::Glob {
        value: "git *".into(),
    };
    for c in [exact, prefix, glob] {
        let json = serde_json::to_string(&c).expect("serialize");
        let back: PermissionRuleContent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, c);
    }
}

#[test]
fn test_content_tags_by_kind() {
    let json = serde_json::to_string(&PermissionRuleContent::Prefix {
        value: "npm".into(),
    })
    .expect("serialize");
    assert!(json.contains("\"kind\""), "tagged by kind: {json}");
    assert!(json.contains("\"prefix\""), "snake_case variant: {json}");
    assert!(json.contains("\"value\""), "field name: {json}");
}

#[test]
fn test_rule_round_trips_content() {
    let rule = PermissionRule {
        action: "bash".into(),
        content: Some(PermissionRuleContent::Prefix {
            value: "npm".into(),
        }),
        effect: PermissionEffect::Allow,
        destination: RuleDestination::User,
    };
    let json = serde_json::to_string(&rule).expect("serialize");
    let back: PermissionRule = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, rule);
}

#[test]
fn test_round_trips_without_content() {
    let rule = PermissionRule {
        action: "bash".into(),
        content: None,
        effect: PermissionEffect::Reject,
        destination: RuleDestination::default(),
    };
    let json = serde_json::to_string(&rule).expect("serialize");
    let back: PermissionRule = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, rule);
    // content is skipped when None.
    assert!(!json.contains("content"), "None content skipped: {json}");
}

#[test]
fn test_rule_destination_defaults_project() {
    // A legacy add that omits destination round-trips to project (the
    // default), so an older client that never set the field still lands
    // in the repo-shared scope.
    let json = r#"{"action":"bash","effect":"allow"}"#;
    let back: PermissionRule = serde_json::from_str(json).expect("deserialize");
    assert_eq!(back.destination, RuleDestination::Project);
}

#[test]
fn test_rule_camel_case_keys() {
    let rule = PermissionRule {
        action: "bash".into(),
        content: None,
        effect: PermissionEffect::Allow,
        destination: RuleDestination::default(),
    };
    let json = serde_json::to_string(&rule).expect("serialize");
    assert!(json.contains("\"action\""), "camelCase action: {json}");
    assert!(json.contains("\"effect\""), "camelCase effect: {json}");
}

#[test]
fn test_decision_entry_round_trips() {
    let entry = PermissionDecisionEntry {
        tool: "bash".into(),
        verdict: "allow".into(),
        scope: "npm install".into(),
        call_id: "call_42".into(),
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    assert!(json.contains("\"callId\""), "camelCase callId: {json}");
    let back: PermissionDecisionEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, entry);
}

#[test]
fn test_permission_decision_decodes() {
    // The server emits the verdict notification params as this JSON shape;
    // the typed struct must decode it (camelCase callId), not the prior
    // snake_case call_id.
    let params = serde_json::json!({
        "callId": "call_7",
        "tool": "bash",
        "verdict": "deny",
        "scope": "rm -rf",
    });
    let entry: PermissionDecisionEntry =
        serde_json::from_value(params).expect("decode server params");
    assert_eq!(entry.tool, "bash");
    assert_eq!(entry.verdict, "deny");
    assert_eq!(entry.call_id, "call_7");
    assert_eq!(entry.scope, "rm -rf");
}

#[test]
fn test_ask_source_round_trips() {
    for s in [
        AskSource::UserRule,
        AskSource::SystemSafety,
        AskSource::Detection,
        AskSource::ToolNative,
    ] {
        let json = serde_json::to_string(&s).expect("serialize");
        let back: AskSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }
}

#[test]
fn test_ask_source_falls_back() {
    // A source a newer engine might emit decodes to Unknown, not an error.
    let back: AskSource = serde_json::from_str("\"yet_another_source\"").expect("fallback");
    assert_eq!(back, AskSource::Unknown);
}

#[test]
fn test_deny_source_round_trips() {
    for s in [DenySource::UserRule, DenySource::Headless] {
        let json = serde_json::to_string(&s).expect("serialize");
        let back: DenySource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }
}

#[test]
fn test_ask_reason_round_trips() {
    let r = AskReason {
        source: AskSource::SystemSafety,
        validator: "protected_path".into(),
        detail: "writing to a protected path".into(),
        containment_note: Some("the fence may still block this".into()),
    };
    let json = serde_json::to_string(&r).expect("serialize");
    assert!(json.contains("\"containmentNote\""), "camelCase: {json}");
    let back: AskReason = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, r);
}

#[test]
fn test_ask_reason_omits_note() {
    // A legacy engine that omits containment_note deserializes fine.
    let json = r#"{"source":"detection","validator":"rm","detail":"x"}"#;
    let back: AskReason = serde_json::from_str(json).expect("deserialize");
    assert_eq!(back.source, AskSource::Detection);
    assert!(back.containment_note.is_none());
}

#[test]
fn test_allow_reason_round_trips() {
    for r in [
        AllowReason::UserRule,
        AllowReason::Consent,
        AllowReason::Containment,
        AllowReason::ModeDefault,
    ] {
        let json = serde_json::to_string(&r).expect("serialize");
        let back: AllowReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }
}
