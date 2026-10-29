//! Tests for this module.

use super::*;
use houyicoder_protocol::tool::{tool_result_failed, tool_result_not_run};

#[test]
fn test_tool_error_carries_prefix() {
    let e = ToolError::Failed("boom".into());
    let v = tool_error_json(&e);
    assert_eq!(v, serde_json::json!({"error": "tool error: boom"}));
}

/// Every fallback payload names why the call did not run and marks itself as
/// unexecuted, so no reader counts it as a verdict on an execution. The
/// reason text stays bare: a real tool error carries a prefix, these do not.
#[test]
fn test_outcomes_mark_not_run() {
    let cases = [
        FallbackToolOutcome::UnknownTool {
            name: "x".into(),
            on_resume: false,
        },
        FallbackToolOutcome::UnknownTool {
            name: "x".into(),
            on_resume: true,
        },
        FallbackToolOutcome::Rejected,
        FallbackToolOutcome::Interrupted,
    ];
    for outcome in cases {
        let value = outcome.to_json();
        let reason = value.get("error").and_then(Value::as_str).unwrap_or("");
        assert!(!reason.is_empty(), "{value} names the reason");
        assert!(
            !reason.starts_with("tool error:"),
            "{value} carries no error prefix"
        );
        assert!(tool_result_not_run(&value), "{value} marks the call unrun");
        assert!(
            !tool_result_failed(&value, "", &Value::Null),
            "{value} is not a failure"
        );
    }
}
