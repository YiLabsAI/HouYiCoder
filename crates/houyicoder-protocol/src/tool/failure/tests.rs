//! Tests for this module.

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

/// git diff reports a difference through its exit status only when the caller
/// asked for that, and every other git failure also exits 1. The flag is what
/// separates the answer from the error.
#[test]
fn test_git_diff_exit_code() {
    let out = serde_json::json!({"success": false, "exit_code": 1});
    assert!(!tool_result_failed(
        &out,
        "bash",
        &bash("git diff --exit-code HEAD~1")
    ));
    assert!(!tool_result_failed(&out, "bash", &bash("git diff --quiet")));
    assert!(
        tool_result_failed(&out, "bash", &bash("git diff HEAD~1")),
        "a plain git diff that exits 1 is an error, not a difference"
    );
    assert!(
        tool_result_failed(&out, "bash", &bash("git checkout --exit-code")),
        "the flag alone must not exempt an unrelated git subcommand"
    );
    // Global options sit between git and the subcommand, and the ones that
    // take a value must not be read as the subcommand name.
    assert!(!tool_result_failed(
        &out,
        "bash",
        &bash("git -C /work/repo diff --exit-code")
    ));
    assert!(!tool_result_failed(
        &out,
        "bash",
        &bash("git --no-pager diff --quiet")
    ));
    assert!(
        tool_result_failed(&out, "bash", &bash("git -C /work/repo status")),
        "a global option does not make an unrelated subcommand a diff"
    );
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

/// A refused call carries the marker the counts separate it by. The marker's
/// presence is the fact; the verdict inside it is not consulted here.
#[test]
fn test_refused_marker_reads() {
    assert!(tool_result_refused(&serde_json::json!({
        "error": "blocked by hook: no writes",
        "not_run": true,
        "refused": { "by": "hook", "verdict": "policy" }
    })));
    assert!(tool_result_refused(&serde_json::json!({
        "error": "rejected by user",
        "not_run": true,
        "refused": { "by": "user", "verdict": "reject" }
    })));
}

/// A refusal recorded by a later engine still reads as one: a verdict name
/// this build does not know says nothing about whether the call was refused.
#[test]
fn test_foreign_verdict_still_refused() {
    assert!(tool_result_refused(&serde_json::json!({
        "not_run": true,
        "refused": { "by": "hook", "verdict": "a later verdict" }
    })));
}

/// A call that never ran for another reason is not a refusal, and neither is
/// an execution failure.
#[test]
fn test_plain_results_are_unrefused() {
    assert!(!tool_result_refused(&serde_json::json!({
        "error": "interrupted by user",
        "not_run": true
    })));
    assert!(!tool_result_refused(
        &serde_json::json!({ "error": "boom" })
    ));
    assert!(!tool_result_refused(&serde_json::json!({ "stdout": "hi" })));
}
