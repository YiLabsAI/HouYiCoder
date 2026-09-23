//! Whether a durable tool result reports a failure.
//!
//! The trajectory header counts failures, and the transcript colors a tool
//! chip, so both must read the same rule: an error key or success false is a
//! failure, and a bash command whose non-zero exit is its own verdict is not.
//! Keeping the rule here, beside the event types it reads, means the two
//! consumers cannot drift apart.

use serde_json::Value;

use super::bash_command::simple_command_word;

/// Whether a tool result's output reports a failure.
///
/// An error key or success false is a failure. For bash, a non-zero exit is
/// not always one: grep exits 1 when there are no matches and diff exits 1
/// when files differ, and both are the command reporting its result rather
/// than failing. The command text (from the call's input) is inspected to
/// recognize those, so they are not counted as failures. Other tools keep the
/// plain error-key rule.
///
/// A result whose call is outside the read window has no tool or input to
/// consult; callers pass an empty tool name and a null input, which applies
/// the plain rule.
pub fn tool_result_failed(output: &Value, tool_name: &str, call_input: &Value) -> bool {
    let has_error = output.get("error").is_some();
    let success_false = output.get("success").and_then(|v| v.as_bool()) == Some(false);
    if !has_error && !success_false {
        return false;
    }
    if tool_name == "bash" && !has_error {
        let exit_code = output
            .get("exit_code")
            .and_then(|c| c.as_i64())
            .unwrap_or(0);
        let command = call_input
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if exit_code != 0 && exit_is_semantic_success(command, exit_code) {
            return false;
        }
    }
    true
}

/// Whether a bash command's non-zero exit is a semantic success (the command
/// did its job), not a failure. grep exits 1 when no matches are found; diff
/// exits 1 when files differ; both are the command reporting a result, not
/// failing. A compound command yields no command word (its exit code belongs
/// to the last stage), so it stays a failure on non-zero.
fn exit_is_semantic_success(command: &str, exit_code: i64) -> bool {
    let Some(word) = simple_command_word(command) else {
        return false;
    };
    match (word, exit_code) {
        ("grep", 1) => true, // no matches — the command succeeded
        ("rg", 1) => true,   // ripgrep — same
        ("diff", 1) => true, // files differ — the command succeeded
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str) -> Value {
        serde_json::json!({ "command": command })
    }

    #[test]
    fn test_error_key_fails() {
        assert!(tool_result_failed(
            &serde_json::json!({"error": "boom"}),
            "bash",
            &Value::Null
        ));
    }

    #[test]
    fn test_success_false_fails() {
        assert!(tool_result_failed(
            &serde_json::json!({"success": false}),
            "read",
            &Value::Null
        ));
    }

    #[test]
    fn test_plain_success_passes() {
        assert!(!tool_result_failed(
            &serde_json::json!({"stdout": "hi", "exit_code": 0}),
            "bash",
            &bash("echo hi")
        ));
    }

    /// A non-zero exit reaches the exemption only when the payload already
    /// reports a failure; a bare exit code is not a failure on its own.
    #[test]
    fn test_bare_exit_code_passes() {
        assert!(!tool_result_failed(
            &serde_json::json!({"stdout": "", "exit_code": 1}),
            "bash",
            &bash("mv a b")
        ));
    }

    #[test]
    fn test_semantic_exit_passes() {
        let out = serde_json::json!({"success": false, "exit_code": 1});
        assert!(!tool_result_failed(&out, "bash", &bash("grep x")));
        assert!(!tool_result_failed(&out, "bash", &bash("rg x")));
        assert!(!tool_result_failed(&out, "bash", &bash("diff a b")));
    }

    #[test]
    fn test_plain_nonzero_fails() {
        let out = serde_json::json!({"success": false, "exit_code": 1});
        assert!(tool_result_failed(&out, "bash", &bash("mv a b")));
        assert!(tool_result_failed(&out, "bash", &bash("make")));
    }

    /// A compound command's exit code belongs to its last stage, so it is not
    /// exempted on the first word's account.
    #[test]
    fn test_compound_not_exempt() {
        let out = serde_json::json!({"success": false, "exit_code": 1});
        assert!(tool_result_failed(&out, "bash", &bash("grep x | head")));
    }

    /// A result whose call is outside the window has nothing to consult, so
    /// the plain rule applies.
    #[test]
    fn test_missing_call_plain_rule() {
        assert!(tool_result_failed(
            &serde_json::json!({"success": false, "exit_code": 1}),
            "",
            &Value::Null
        ));
        assert!(tool_result_failed(
            &serde_json::json!({"error": "boom"}),
            "",
            &Value::Null
        ));
    }

    /// An error key is a failure even when the command would be exempt: the
    /// exception covers a non-zero exit, not a reported error.
    #[test]
    fn test_error_beats_exception() {
        assert!(tool_result_failed(
            &serde_json::json!({"error": "boom", "exit_code": 1}),
            "bash",
            &bash("grep x")
        ));
    }
}
