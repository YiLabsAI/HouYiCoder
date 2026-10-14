//! Tests for this module.

use super::*;
use pollster::block_on;

fn one_question() -> Value {
    json!({
        "questions": [{
            "question": "Which library for date formatting?",
            "header": "Library",
            "options": [
                {"label": "chrono", "description": "mature, heavy"},
                {"label": "time", "description": "lighter, newer"}
            ],
            "multiSelect": false
        }]
    })
}

#[test]
fn test_execute_refuses_without_approval() {
    // Reaching execute means the approval path was bypassed; fail closed.
    let tool = AskUserQuestionTool::new();
    let result = block_on(tool.execute(ToolCtx::new("test"), one_question()));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.message().contains("human input"));
}

#[test]
fn test_authorized_formats_answer_string() {
    let tool = AskUserQuestionTool::new();
    // The UI injects the answers into the input before resume runs this.
    let input = json!({
        "questions": [{
            "question": "Which library for date formatting?",
            "header": "Library",
            "options": [
                {"label": "chrono", "description": "mature, heavy"},
                {"label": "time", "description": "lighter, newer"}
            ],
            "multiSelect": false
        }],
        "answers": {"Which library for date formatting?": "chrono"}
    });
    let out = block_on(tool.execute_authorized(ToolCtx::new("test"), input)).unwrap();
    assert_eq!(out["answered"], true);
    let summary = out["summary"].as_str().expect("summary present");
    assert!(summary.contains("User has answered"), "{summary}");
    assert!(summary.contains("chrono"), "{summary}");
    assert_eq!(
        out["answers"]["Which library for date formatting?"],
        "chrono"
    );
}

#[test]
fn test_format_handles_missing_answers() {
    // No answers injected yet: the result is still well-formed so the
    // model sees a (empty) answer rather than a malformed payload.
    let out = format_answer_result(&one_question());
    assert_eq!(out["answered"], true);
    let summary = out["summary"].as_str().expect("summary present");
    assert!(summary.contains("User has answered"));
    assert!(summary.contains("=\"\""));
}

#[test]
fn test_format_multiple_questions() {
    let input = json!({
        "questions": [
            {"question": "q1", "header": "h1", "options": [{"label":"a","description":"x"},{"label":"b","description":"y"}], "multiSelect": false},
            {"question": "q2", "header": "h2", "options": [{"label":"c","description":"x"},{"label":"d","description":"y"}], "multiSelect": true}
        ],
        "answers": {"q1": "a", "q2": "c, d"}
    });
    let out = format_answer_result(&input);
    let summary = out["summary"].as_str().expect("summary present");
    assert!(summary.contains("q1"), "{summary}");
    assert!(summary.contains("q2"), "{summary}");
    assert!(summary.contains("c, d"), "{summary}");
}

#[test]
fn test_schema_excludes_answers_field() {
    // The model sends questions only; answers are added by the UI, not the
    // schema the model sees (so the model cannot pre-fill answers).
    let tool = AskUserQuestionTool::new();
    let schema = tool.input_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let props = schema["properties"].as_object().expect("properties");
    assert!(props.contains_key("questions"));
    assert!(!props.contains_key("answers"));
    let q_schema = &props["questions"]["items"];
    assert_eq!(q_schema["additionalProperties"], false);
    let required = q_schema["required"].as_array().expect("required");
    assert!(required.iter().any(|r| r == "question"));
    assert!(required.iter().any(|r| r == "header"));
    assert!(required.iter().any(|r| r == "options"));
}

#[test]
fn test_capability_flags() {
    let tool = AskUserQuestionTool::new();
    // Read-only: pauses for input, touches no workspace state.
    assert!(tool.is_read_only());
    assert!(tool.is_concurrency_safe());
    assert!(!tool.is_destructive());
    // Always pauses — the model cannot run this without a human.
    assert!(tool.requires_approval());
}
