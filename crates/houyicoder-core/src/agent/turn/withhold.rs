//! What a turn's shell checks do when a file edit they follow did not
//! complete. A check whose edit wrote nothing is answered as skipped; one
//! behind an edit still awaiting a verdict keeps its place for the resume that
//! decides it, and is answered as skipped instead when it would run in the
//! same batch, with the model sending it again once the edit lands. One rule
//! decides this for every path that releases one response's calls.

use crate::agent::fallback::FallbackToolOutcome;
use crate::observability::tool_failure_reason;
use houyicoder_api::tool::{is_shell_tool, writes_files};
use houyicoder_protocol::tool::tool_result_not_run;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Whether a call ran and failed: a real execution failure, not a call the
/// host answered without running it.
pub(crate) fn ran_and_failed(output: &Value) -> bool {
    !tool_result_not_run(output) && tool_failure_reason(output).is_some()
}

/// What a recorded result says about the call that produced it, read from the
/// log on the resume path. A call the host answered without running it counts
/// as missed: it wrote nothing, so a check sent after it would read source it
/// never changed. That is deliberately stricter than the live path, which
/// keeps the check after an interrupted edit because the run is already ending
/// there, and a skipped answer would state a verdict on work the user stopped.
pub(crate) fn recorded_outcome(output: &Value) -> EditOutcome {
    if tool_result_not_run(output) || tool_failure_reason(output).is_some() {
        EditOutcome::Missed
    } else {
        EditOutcome::Completed
    }
}

/// The result recorded for a shell call the turn did not let run. It carries
/// the not-run marker: the call was answered without executing, so the
/// outcome counts and the transcript must not read it as a failed run.
pub(crate) fn skipped_after_edit() -> Value {
    FallbackToolOutcome::SkippedAfterEdit.to_json()
}

/// What is known about a file edit's outcome, as far as the checks sent after
/// it are concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditOutcome {
    /// The edit ran and wrote its file.
    Completed,
    /// The edit is over and wrote nothing: refused, declined, unresolvable,
    /// malformed, interrupted, or failed while running.
    Missed,
    /// The edit awaits a verdict and may still write.
    Pending,
}

/// What a check emitted at this point of a response must do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CheckAction {
    /// No edit before it missed, so it runs.
    Run,
    /// An edit before it awaits a verdict: the check must not run yet, but it
    /// keeps its place for the resume that decides that edit.
    Wait,
    /// An edit before it wrote nothing: answer it as skipped.
    Skip,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Anchor {
    Missed,
    Pending,
}

/// The dependency state of the checks in one model response, advanced over
/// its calls in emission order.
///
/// The first edit that did not complete holds the plan for every check sent
/// after it. A later edit that succeeds does not release them: its success
/// says nothing about whether the earlier edit's checks were testing the
/// source the model meant to write.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CheckPlan {
    anchor: Option<Anchor>,
}

impl CheckPlan {
    /// Record what is known about one call. A call that is not a file edit,
    /// or an edit that completed, leaves the plan unchanged.
    pub(crate) fn observe(&mut self, tool: &str, outcome: EditOutcome) {
        if self.anchor.is_some() || !writes_files(tool) {
            return;
        }
        match outcome {
            EditOutcome::Completed => {}
            EditOutcome::Missed => self.anchor = Some(Anchor::Missed),
            EditOutcome::Pending => self.anchor = Some(Anchor::Pending),
        }
    }

    /// What this call must do now. Only a shell is held back: every other
    /// tool runs whatever the edits before it did.
    pub(crate) fn action(&self, tool: &str) -> CheckAction {
        if !is_shell_tool(tool) {
            return CheckAction::Run;
        }
        match self.anchor {
            None => CheckAction::Run,
            Some(Anchor::Missed) => CheckAction::Skip,
            Some(Anchor::Pending) => CheckAction::Wait,
        }
    }
}

/// The checks of one response that must not run yet, split by what each one
/// keeps: an answer now, or its place in the batch.
#[derive(Debug, Default)]
pub(crate) struct Withheld {
    /// Answered as skipped, because the edit they follow wrote nothing.
    pub(crate) skip: HashSet<String>,
    /// Kept for a later resume, because the edit they follow is undecided.
    pub(crate) wait: HashSet<String>,
}

/// What each check of one response must do, given the state of every call in
/// it.
pub(crate) fn check_actions(
    emitted_calls: &[(String, String)],
    outcomes: &HashMap<String, EditOutcome>,
) -> Withheld {
    let mut plan = CheckPlan::default();
    let mut withheld = Withheld::default();
    for (id, tool) in emitted_calls {
        match plan.action(tool) {
            CheckAction::Run => {}
            CheckAction::Wait => {
                withheld.wait.insert(id.clone());
            }
            CheckAction::Skip => {
                withheld.skip.insert(id.clone());
            }
        }
        let outcome = outcomes.get(id).copied().unwrap_or(EditOutcome::Completed);
        plan.observe(tool, outcome);
    }
    withheld
}

/// What each emitted call leaves behind for the checks sent after it: a call
/// that is waiting for a verdict has not written anything yet, and every
/// other call is either done or settled as not done.
pub(crate) fn outcomes(
    emitted_calls: &[(String, String)],
    did_not_run: &HashSet<String>,
    awaiting: &HashSet<String>,
) -> HashMap<String, EditOutcome> {
    emitted_calls
        .iter()
        .map(|(id, _)| {
            let outcome = if awaiting.contains(id) {
                EditOutcome::Pending
            } else if did_not_run.contains(id) {
                EditOutcome::Missed
            } else {
                EditOutcome::Completed
            };
            (id.clone(), outcome)
        })
        .collect()
}

/// The ids of the calls that ran a file edit and failed. Their outcome is
/// settled only after execution, so the checks sent after them are withheld
/// then.
pub(crate) fn failed_edits(
    results: &[(String, Value)],
    call_names: &HashMap<String, String>,
) -> Vec<String> {
    results
        .iter()
        .filter(|(id, output)| {
            call_names.get(id).is_some_and(|name| writes_files(name)) && ran_and_failed(output)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Remove the withheld checks from a dispatch list and return the answer each
/// keeps: its call id and the skipped result.
pub(crate) fn take_withheld<T>(
    calls: &mut Vec<T>,
    withheld: &HashSet<String>,
    id: impl Fn(&T) -> &str,
) -> Vec<(String, Value)> {
    let mut answers: Vec<(String, Value)> = Vec::new();
    calls.retain(|call| {
        let call_id = id(call);
        if withheld.contains(call_id) {
            answers.push((call_id.to_string(), skipped_after_edit()));
            false
        } else {
            true
        }
    });
    answers
}

#[cfg(test)]
mod tests;
