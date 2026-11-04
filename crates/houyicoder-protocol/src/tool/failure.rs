//! Whether a durable tool result reports a failure.
//!
//! The trajectory header counts failures, and the transcript colors a tool
//! chip, so both must read the same rule: an error key or success false is a
//! failure, and a bash command whose non-zero exit is its own verdict is not.
//! A result the runner wrote for a call that never ran is neither, and the
//! marker it carries says so; a call someone refused carries a second marker,
//! because a refusal is a decision about the call rather than a missing
//! result. Keeping the rule here, beside the event types it reads, means the
//! consumers cannot drift apart.

use serde_json::Value;

use super::bash_command::simple_command_word;

/// How much of a command a fold keeps for the failure rule. One entry exists
/// per call still awaiting its result, so a command carrying a long body must
/// not be carried whole.
const RETAINED_COMMAND_CAP: usize = 512;

/// The command text a later failure verdict reads, reduced to what that
/// verdict can use.
///
/// A compound command yields None: its exit status belongs to its last stage,
/// so the rule refuses it. A command carrying a line break or more text than
/// RETAINED_COMMAND_CAP is reduced to the least text that still yields the
/// verdict the whole command yields: the canonical git diff invocation when
/// the command is one, and its command word otherwise. Everything else is
/// kept whole.
///
/// The reduction is only sound because the failure rule reads no more than
/// this: the command word, and for git the subcommand and its exit flag. A
/// fold that stores this text therefore judges a call exactly as a reader that
/// passes the call input straight to the rule does.
pub fn retained_command(command: &str) -> Option<String> {
    let trimmed = command.trim();
    let word = simple_command_word(trimmed)?;
    if trimmed.contains('\n') || trimmed.len() > RETAINED_COMMAND_CAP {
        // Only a git invocation can be recognised by its subcommand and flag;
        // any other command word keeps its own verdict.
        if word == "git" && git_diff_reports_change(trimmed) {
            return Some("git diff --exit-code".to_string());
        }
        return Some(word.to_string());
    }
    Some(trimmed.to_string())
}

/// The call input a failure verdict reads, rebuilt from retained command text.
///
/// A call that kept no command passes null, which the rule treats as a call
/// it knows nothing about.
pub fn failure_rule_input(command: Option<&str>) -> Value {
    match command {
        Some(command) => serde_json::json!({ "command": command }),
        None => Value::Null,
    }
}

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
    if tool_result_not_run(output) {
        return false;
    }
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

/// Whether a bash command's exit status reports success, including the exits
/// that are the command's own verdict rather than a failure.
///
/// A background task reads its ending through this, and a tool result reads
/// the same rule, so one finished command cannot count as a success on one
/// path and a failure on the other.
pub fn bash_exit_succeeded(command: &str, exit_code: i64) -> bool {
    exit_code == 0 || exit_is_semantic_success(command, exit_code)
}

/// Whether a durable tool result says its call never ran.
///
/// The runner answers a call it did not execute — the turn ended while the
/// call was pending, the user rejected it, the tool is gone — with a result
/// that names the reason. That text is a fact about the call, not a verdict
/// on an execution: nothing ran, so the result is neither a success nor a
/// failure and a count of either must leave it out.
pub fn tool_result_not_run(output: &Value) -> bool {
    output.get("not_run").and_then(Value::as_bool) == Some(true)
}

/// Whether a durable tool result says the call was refused before it ran.
///
/// A refusal is a decision against the call: the user declined the approval
/// card, or a hook denied, asked about, or gave feedback on it. The runner
/// records it under a refused object naming who refused the call and which
/// verdict the refusal came from. A refusal is also a call that never ran,
/// so a reader that separates the two reads this first.
///
/// The test is the presence of the marker, not a recognized verdict inside
/// it: a refusal recorded by a later engine still counts as one here, and a
/// reader that also wants the verdict reads the object itself.
pub fn tool_result_refused(output: &Value) -> bool {
    output
        .get("refused")
        .is_some_and(|refused| refused.is_object())
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
        // git diff only reports a difference through its exit status when
        // asked to, and every other git failure also exits 1, so both the
        // subcommand and the flag have to be there for the status to be an
        // answer rather than an error.
        ("git", 1) => git_diff_reports_change(command),
        _ => false,
    }
}

/// Whether a git invocation is a diff that was asked to report a change
/// through its exit status. Global options sit between git and the
/// subcommand, and the ones that take a value would otherwise be read as the
/// subcommand name.
fn git_diff_reports_change(command: &str) -> bool {
    let mut words = command.split_whitespace().skip(1).peekable();
    let mut subcommand = None;
    while let Some(word) = words.next() {
        match word {
            // Global options that consume the following word.
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => {
                words.next();
            }
            w if w.starts_with('-') => {}
            w => {
                subcommand = Some(w);
                break;
            }
        }
    }
    subcommand == Some("diff") && words.any(|w| w == "--exit-code" || w == "--quiet")
}

#[cfg(test)]
mod tests;
