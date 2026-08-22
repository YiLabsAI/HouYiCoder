//! Peer tests for records.rs — the semantic-error-judgment tests split out
//! so records.rs stays under the file-size gate.

use crate::records::ToolOutcome;

// A non-zero exit is not always an error. grep exits 1 when no matches are
// found; diff exits 1 when files differ. Both are the command succeeding at
// its semantic job, not failing. Without this the grep no-match result
// renders red — the user sees a "failure" that is the command's normal
// output.
#[test]
fn test_grep_no_match_success() {
    let out = serde_json::json!({
        "stdout": "",
        "stderr": "",
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Success,
        "grep exit 1 (no match) is semantic success"
    );
}

#[test]
fn test_diff_differ_success() {
    let out = serde_json::json!({
        "stdout": "1c1\n< a\n---\n> b\n",
        "stderr": "",
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "diff a.txt b.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Success,
        "diff exit 1 (files differ) is semantic success"
    );
}

#[test]
fn test_grep_exit_two_error() {
    // grep exit 2 is a real error (file not found, bad option). Only
    // exit 1 (no match) is semantic success.
    let out = serde_json::json!({
        "stdout": "",
        "stderr": "grep: bar.txt: No such file",
        "exit_code": 2,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "grep exit 2 is a real error"
    );
}

#[test]
fn test_false_command_is_error() {
    // The false command exits 1 with no semantic success — it is a real failure.
    let out = serde_json::json!({
        "stdout": "",
        "stderr": "",
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "false"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "false exit 1 is a real error"
    );
}

#[test]
fn test_env_prefix_grep_success() {
    // A leading env assignment (GREP_COLOR=always grep ...) must not
    // hide the grep command from semantic recognition.
    let out = serde_json::json!({
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "GREP_COLOR=always grep foo bar.txt"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Success,
        "env-prefixed grep exit 1 is semantic success"
    );
}

#[test]
fn test_error_key_still_error() {
    // A non-bash tool with an error key is always an error (the
    // semantic-success rule only applies to bash).
    let out = serde_json::json!({"error": "permission denied"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "read", &serde_json::Value::Null),
        ToolOutcome::Error
    );
}

/// A pipeline's exit code belongs to the LAST stage, not the first. "grep
/// foo | head" exiting 1 means head failed, not grep found no match —
/// recognizing grep here would mis-color a real head failure as success.
/// The semantic-success check must bail out on shell control operators.
#[test]
fn test_pipeline_not_semantic_success() {
    let out = serde_json::json!({
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt | head"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "pipeline exit 1 is not auto-success even if first word is grep"
    );
}

/// A compound command (grep foo; echo done) has the exit code of the LAST
/// stage. Same bail-out as pipelines.
#[test]
fn test_compound_not_semantic_success() {
    let out = serde_json::json!({
        "exit_code": 1,
        "success": false,
    });
    let input = serde_json::json!({"command": "grep foo bar.txt; false"});
    assert_eq!(
        ToolOutcome::from_output_with(&out, "bash", &input),
        ToolOutcome::Error,
        "compound command exit 1 is not auto-success"
    );
}
