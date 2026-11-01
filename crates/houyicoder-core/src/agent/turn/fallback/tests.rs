//! Tests for this module.

use super::*;
use houyicoder_protocol::tool::{tool_result_failed, tool_result_not_run, tool_result_refused};

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
        FallbackToolOutcome::SkippedAfterEdit,
    ];
    for outcome in cases {
        let value = outcome.to_json();
        let reason = value.get("error").and_then(Value::as_str).unwrap_or("");
        assert!(!reason.is_empty(), "{value} names the reason");
        assert!(
            !reason.starts_with("tool error:"),
            "{value} carries no error prefix"
        );
        assert!(
            tool_result_not_run(&value),
            "{value} marks the call as not run"
        );
        assert!(
            !tool_result_failed(&value, "", &Value::Null),
            "{value} is not a failure"
        );
    }
}

/// Of the fallback outcomes only a user rejection is a refusal: the run
/// ending, a missing tool, and a skipped check are not decisions against
/// the call, so they must not carry the refusal marker.
#[test]
fn test_only_rejection_refused() {
    let rejected = FallbackToolOutcome::Rejected.to_json();
    assert!(tool_result_refused(&rejected));
    assert_eq!(
        rejected.pointer("/refused/by").and_then(Value::as_str),
        Some("user")
    );
    assert_eq!(
        rejected.pointer("/refused/verdict").and_then(Value::as_str),
        Some("reject")
    );
    let unrefused = [
        FallbackToolOutcome::Interrupted,
        FallbackToolOutcome::SkippedAfterEdit,
        FallbackToolOutcome::UnknownTool {
            name: "x".into(),
            on_resume: false,
        },
    ];
    for outcome in unrefused {
        assert!(!tool_result_refused(&outcome.to_json()));
    }
}
