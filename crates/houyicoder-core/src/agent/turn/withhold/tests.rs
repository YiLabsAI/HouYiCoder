//! Tests for this module.

use super::*;
use crate::agent::fallback::SKIPPED_AFTER_EDIT;
use crate::agent::turn::resolve::ToolCallPlan;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::tool::tool_result_not_run;
use std::sync::Arc;

/// A tool that carries a fixed name, so a plan can stand for edit, read, or
/// bash without three separate stubs.
struct NamedTool(&'static str);

impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test stub"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(serde_json::json!({})) })
    }
}

fn named_plan(id: &str, name: &'static str) -> ToolCallPlan {
    ToolCallPlan::new(id.to_string(), Arc::new(NamedTool(name)), Value::Null)
}

fn emitted(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

fn ids(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

/// What one response's checks must do when the listed calls did not run.
fn settled(pairs: &[(&str, &str)], did_not_run: &[&str]) -> Withheld {
    let calls = emitted(pairs);
    let outcomes = outcomes(&calls, &ids(did_not_run), &HashSet::new());
    check_actions(&calls, &outcomes)
}

/// Every tool that writes a file withdraws the shell calls emitted after it.
/// The model sends a write with a check in one turn; when the write does not
/// run the check would read source that was never written and report a
/// failure about the wrong thing.
#[test]
fn test_skipped_edit_withholds_shell() {
    for name in ["edit", "write", "multiedit", "patch", "str_replace"] {
        let withheld = settled(&[("e1", name), ("b1", "bash")], &["e1"]);

        assert_eq!(withheld.skip, ids(&["b1"]), "{name}");
    }
}

/// The family match folds case, because a provider spells the name it was
/// given. A check behind an edit still depends on it whichever case the model
/// wrote.
#[test]
fn test_edit_name_any_case() {
    for name in ["Edit", "WRITE", "MultiEdit"] {
        let withheld = settled(&[("e1", name), ("b1", "bash")], &["e1"]);

        assert_eq!(withheld.skip, ids(&["b1"]), "{name}");
    }
}

/// The withdrawn call keeps an answer, and that answer is the not-run result:
/// the outcome counts must not read a call that never ran as a failure.
#[test]
fn test_withheld_check_keeps_answer() {
    let mut exec = vec![named_plan("e1", "edit"), named_plan("b1", "bash")];
    let withheld = settled(&[("e1", "edit"), ("b1", "bash")], &["e1"]);

    let answers = take_withheld(&mut exec, &withheld.skip, |plan| plan.id.as_str());

    assert_eq!(exec.len(), 1, "the edit plan itself is untouched");
    assert_eq!(answers.len(), 1, "the withheld call still has an answer");
    assert_eq!(answers[0].0, "b1");
    assert_eq!(answers[0].1["error"], SKIPPED_AFTER_EDIT);
    assert!(
        tool_result_not_run(&answers[0].1),
        "the answer is not a run"
    );
}

/// A call that did not run but is not a file edit withdraws nothing. A denied
/// read must not take the rest of the turn's work with it.
#[test]
fn test_skipped_read_keeps_shell() {
    let withheld = settled(&[("r1", "read"), ("b1", "bash")], &["r1"]);

    assert!(
        withheld.skip.is_empty(),
        "an unrelated refusal withdraws nothing"
    );
}

/// A shell call emitted before the edit that did not run does not depend on
/// it, so it still runs.
#[test]
fn test_early_shell_kept() {
    let withheld = settled(&[("b1", "bash"), ("e1", "edit")], &["e1"]);

    assert!(withheld.skip.is_empty(), "the earlier command still runs");
}

/// No call that failed to run means no withdrawal, so a write and its check
/// still run in the same turn.
#[test]
fn test_completed_edit_keeps_shell() {
    let withheld = settled(&[("e1", "edit"), ("b1", "bash")], &[]);

    assert!(
        withheld.skip.is_empty(),
        "an unrefused turn runs what it queued"
    );
}

/// A check emitted after another call is still behind the edit: the
/// dependency is the emission position, not the immediate predecessor.
#[test]
fn test_later_check_withholds_shell() {
    let withheld = settled(&[("e1", "edit"), ("r1", "read"), ("b1", "bash")], &["e1"]);

    assert_eq!(withheld.skip, ids(&["b1"]));
}

/// A check behind an edit that is still undecided waits instead of being
/// answered: approving the edit must release it without another model
/// response.
#[test]
fn test_pending_edit_holds_check() {
    let calls = emitted(&[("e1", "edit"), ("b1", "bash")]);
    let outcomes = outcomes(&calls, &HashSet::new(), &ids(&["e1"]));

    let withheld = check_actions(&calls, &outcomes);

    assert_eq!(withheld.wait, ids(&["b1"]));
    assert!(withheld.skip.is_empty());
}

/// An edit that writes nothing holds the checks after it wherever it sits in
/// the response. The first case shows that a later success cannot release the
/// anchor: that success says nothing about whether the earlier check was
/// testing the source the model meant to write. The second covers the reverse
/// order: a success that came before says nothing about the source the edit
/// later failed to write.
#[test]
fn test_any_missed_edit_withholds() {
    let pairs = [("e1", "edit"), ("e2", "edit"), ("b1", "bash")];

    for missed in ["e1", "e2"] {
        let withheld = settled(&pairs, &[missed]);

        assert_eq!(withheld.skip, ids(&["b1"]), "{missed}");
    }
}
