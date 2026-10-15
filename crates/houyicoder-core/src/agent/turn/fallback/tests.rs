//! Tests for this module.

use super::*;

#[test]
fn test_tool_error_carries_prefix() {
    let e = ToolError::Failed("boom".into());
    let v = tool_error_json(&e);
    assert_eq!(v, serde_json::json!({"error": "tool error: boom"}));
}

#[test]
fn test_outcomes_have_no_prefix() {
    assert_eq!(
        FallbackToolOutcome::UnknownTool {
            name: "x".into(),
            on_resume: false
        }
        .to_json(),
        serde_json::json!({"error": "unknown tool: x"})
    );
    assert_eq!(
        FallbackToolOutcome::UnknownTool {
            name: "x".into(),
            on_resume: true
        }
        .to_json(),
        serde_json::json!({"error": "unknown tool on resume"})
    );
    assert_eq!(
        FallbackToolOutcome::Rejected.to_json(),
        serde_json::json!({"error": "rejected by user"})
    );
    assert_eq!(
        FallbackToolOutcome::Interrupted.to_json(),
        serde_json::json!({"error": "interrupted by user"})
    );
}
