//! Widthdrawal of the shell calls a turn queued beside a refused file edit.
//!
//! The tool guide tells the model to send an edit and its check in one turn.
//! When the edit is refused the check is left pointing at source that was
//! never written: it fails for the wrong reason, and the model then spends
//! turns chasing a fault in code nobody changed. Withholding the shell calls
//! leaves the refusal as the turn's only outcome, which is the fact the model
//! needs to act on.

use crate::agent::turn::resolve::ToolCallPlan;
use serde_json::Value;
use std::collections::HashMap;

/// Whether a tool writes a file, so refusing it withdraws the checks.
fn edits_a_file(name: &str) -> bool {
    matches!(name, "edit" | "write" | "multiedit")
}

/// Drop the shell calls this turn queued beside a refused file edit.
///
/// The refusal is known only by call id, so the tool name comes from the
/// names the turn already resolved. A refusal of any other kind withdraws
/// nothing: a denied read must not take the turn's remaining work with it.
pub(crate) fn withhold_shell_after_refused_edit(
    exec: &mut Vec<ToolCallPlan>,
    blocked: &[(String, Value)],
    call_names: &HashMap<String, String>,
) {
    let refused_edit = blocked.iter().any(|(id, _)| {
        call_names
            .get(id)
            .is_some_and(|name| edits_a_file(name.as_str()))
    });
    if refused_edit {
        exec.retain(|plan| plan.tool.name() != "bash");
    }
}
