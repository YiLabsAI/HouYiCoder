//! Tests for this module.

use super::*;
use houyicoder_protocol::llm::OutputItem;

#[test]
fn test_extract_final_text_takes() {
    let out = vec![
        OutputItem::Text {
            text: "first".into(),
        },
        OutputItem::ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            input: serde_json::json!({}),
        },
        OutputItem::Text {
            text: "final answer".into(),
        },
    ];
    assert_eq!(extract_final_text(&out).as_deref(), Some("final answer"));
}

#[test]
fn test_extract_final_text_none() {
    let out = vec![OutputItem::ToolCall {
        id: "c1".into(),
        name: "echo".into(),
        input: serde_json::json!({}),
    }];
    assert!(extract_final_text(&out).is_none());
}

#[test]
fn test_approval_decision_constructors() {
    assert!(ApprovalDecision::approve("c1").approved);
    assert!(!ApprovalDecision::reject("c1").approved);
    // Plain approve/reject carry no updated input.
    assert!(ApprovalDecision::approve("c1").updated_input.is_none());
    assert!(ApprovalDecision::reject("c1").updated_input.is_none());
}

#[test]
fn test_decision_carries_updated_input() {
    // AskUserQuestion answers ride on the decision so resume() runs the
    // tool with the answer-populated input, not the model's original.
    let d = ApprovalDecision::approve_with_input(
        "c1",
        serde_json::json!({"questions": [], "answers": {"q": "a"}}),
    );
    assert!(d.approved);
    let input = d.updated_input.expect("updated input present");
    assert_eq!(input["answers"]["q"], "a");
}
