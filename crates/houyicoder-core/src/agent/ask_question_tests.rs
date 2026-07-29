//! Cross-layer tests for the AskUserQuestion tool's run/resume path: the model
//! calls the tool, the loop pauses on Interruption, the user answers, resume
//! injects the answer via an answer-carrying ApprovalDecision, and the tool
//! result the model sees carries the formatted summary. These assert the real
//! runner wiring, not the tool's execute_authorized in isolation.

use super::tests::runner_with;
use crate::provider::test_support::FakeProvider;
use houyicoder_context::TurnEventKind;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

use super::*;

#[tokio::test]
async fn test_ask_question_resume_answer() {
    // Cross-layer E2E (the verify hard-requirement): the model calls
    // AskUserQuestion -> resolve_turn collects it as an Interruption
    // (requires_approval) -> resume with an answer-populated ApprovalDecision
    // -> apply_decisions runs execute_authorized with the updated input -> the
    // ToolResult event carries the formatted summary the model sees, and the
    // run continues to FinalOutput. The isolated tests bypass this path (they
    // call execute_authorized directly with a hand-built input); this one
    // asserts the real runner wires the answer through to the terminal side.
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "AskUserQuestion".into(),
                input: serde_json::json!({
                    "questions": [{
                        "question": "Which library?",
                        "header": "Library",
                        "options": [
                            {"label": "chrono", "description": "mature"},
                            {"label": "time", "description": "lighter"}
                        ],
                        "multiSelect": false
                    }]
                }),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "got it, using chrono".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(AskUserQuestionTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "pick a date lib".into()).await.unwrap();
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        _ => panic!("expected interruption for AskUserQuestion"),
    };
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].tool_name, "AskUserQuestion");
    // The user picks chrono: build the answer-populated input the UI would.
    let mut updated = approvals[0].input.clone();
    if let serde_json::Value::Object(ref mut obj) = updated {
        obj.insert(
            "answers".into(),
            serde_json::json!({"Which library?": "chrono"}),
        );
    }
    let decisions = vec![ApprovalDecision::approve_with_input(
        &approvals[0].call_id,
        updated,
    )];
    let resumed = runner.resume(session, &decisions).await.unwrap();
    match resumed.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "got it, using chrono"),
        other => panic!("expected final output, got {other:?}"),
    }
    // Terminal-side assertion: the ToolResult the runner appended carries the
    // formatted summary (the model saw the answer, not a placeholder).
    let events = runner.store().replay(session).await.expect("replay");
    let result_output = events
        .iter()
        .find_map(|e| match &e.kind {
            TurnEventKind::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a ToolResult event");
    let summary = result_output
        .get("summary")
        .and_then(|v| v.as_str())
        .expect("summary field");
    assert!(summary.contains("User has answered"), "{summary}");
    assert!(summary.contains("chrono"), "{summary}");
}
