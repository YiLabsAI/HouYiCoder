//! Tests for this module.

use super::*;

#[test]
fn test_final_output_round_trips() {
    let r = RunResult {
        outcome: RunOutcome::FinalOutput {
            content: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        },
        turns: 3,
        usage: Usage::default(),
        stop_reason: StopReason::EndTurn,
    };
    let json = serde_json::to_string(&r).expect("serialize");
    let back: RunResult = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.turns, 3);
    assert_eq!(back.stop_reason, StopReason::EndTurn);
    assert!(matches!(back.outcome, RunOutcome::FinalOutput { .. }));
}

#[test]
fn test_approval_request_round_trips() {
    // A delegation-sourced ask serializes the child origin and survives a
    // round-trip so the frontend reads which delegation the ask came from.
    let ask = ApprovalRequest {
        call_id: "c1".into(),
        tool_name: "bash".into(),
        input: serde_json::json!({"command": "ls"}),
        options: Vec::new(),
        reason: None,
        delegation: Some(DelegationSource {
            child_id: "child-1".into(),
            subagent_type: "explore".into(),
        }),
    };
    let json = serde_json::to_string(&ask).expect("serialize");
    assert!(json.contains("delegation"), "delegation serialized: {json}");
    assert!(json.contains("child-1"));
    let back: ApprovalRequest = serde_json::from_str(&json).expect("deserialize");
    let d = back.delegation.as_ref().expect("delegation survived");
    assert_eq!(d.child_id, "child-1");
    assert_eq!(d.subagent_type, "explore");

    // None delegation is omitted (skip-when-none) so an old client or
    // server that does not know the field still round-trips the rest.
    let none_ask = ApprovalRequest {
        delegation: None,
        ..ask.clone()
    };
    let none_json = serde_json::to_string(&none_ask).expect("serialize");
    assert!(
        !none_json.contains("delegation"),
        "None omitted: {none_json}"
    );
}

#[test]
fn test_content_block_round_trips() {
    // Text serializes as {"type":"text","text":"..."} (internally tagged
    // by type), the base agent protocol content block shape the
    // cross-decode fixture gates.
    let t = ContentBlock::Text { text: "hi".into() };
    let json = serde_json::to_string(&t).expect("serialize");
    assert_eq!(json, r#"{"type":"text","text":"hi"}"#);
    let image = ContentBlock::Image {
        data: "b64".into(),
        mime_type: "image/png".into(),
    };
    let ijson = serde_json::to_string(&image).expect("serialize");
    assert_eq!(
        ijson,
        r#"{"type":"image","data":"b64","mimeType":"image/png"}"#
    );
}

#[test]
fn test_decodes_all_stop_reasons() {
    // Cross-decode parity: a peer may emit any of the five values; the
    // wire must decode all of them even though we only emit three today.
    for raw in [
        r#""end_turn""#,
        r#""max_tokens""#,
        r#""max_turn_requests""#,
        r#""refusal""#,
        r#""cancelled""#,
    ] {
        let _: StopReason = serde_json::from_str(raw).expect("decode base value");
    }
}

#[test]
fn test_outcome_tag_serializes() {
    let r = RunOutcome::Interrupted {
        reason: "esc".to_string(),
    };
    let json = serde_json::to_string(&r).expect("serialize");
    assert!(
        json.contains("\"type\":\"interrupted\""),
        "outcome tagged snake_case: {json}"
    );
}

#[test]
fn test_decision_round_trips() {
    let d = ApprovalDecision {
        call_id: "c1".to_string(),
        approved: false,
        updated_input: None,
        scope: "once".to_string(),
    };
    let json = serde_json::to_string(&d).expect("serialize");
    let back: ApprovalDecision = serde_json::from_str(&json).expect("deserialize");
    assert!(!back.approved);
    assert_eq!(back.call_id, "c1");
}

#[test]
fn test_error_round_trips() {
    let e = RunError {
        category: "provider_exhausted".to_string(),
        message: "provider exhausted: timeout".to_string(),
    };
    let json = serde_json::to_string(&e).expect("serialize");
    assert!(json.contains("\"category\""), "key is category: {json}");
    assert!(!json.contains("\"kind\""), "old key is gone: {json}");
    let back: RunError = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.category, "provider_exhausted");
    assert!(back.message.contains("timeout"));
}

#[test]
fn test_run_error_serializes_exactly() {
    let e = RunError {
        category: "provider_exhausted".to_string(),
        message: "provider exhausted: timeout".to_string(),
    };
    let json = serde_json::to_string(&e).expect("serialize");
    assert_eq!(
        json,
        r#"{"category":"provider_exhausted","message":"provider exhausted: timeout"}"#
    );
}
