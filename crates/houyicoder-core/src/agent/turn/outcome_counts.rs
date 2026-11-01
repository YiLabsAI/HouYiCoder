//! Count tool outcomes (calls / ok / err / refused) for a batch of results.
//! Split from call.rs so that file stays under the size gate.

use crate::observability;
use houyicoder_protocol::tool::{tool_result_not_run, tool_result_refused};

/// Named counts for a batch of tool outcomes. A tuple (calls, ok, err,
/// refused) would let a return-order swap compile silently; the named struct
/// makes the producer's field assignment unambiguous.
///
/// A call answered without executing it still counts in calls, because this
/// tally answers what the model asked for. The per-tool observability tally
/// answers what each tool did and drops such calls entirely, so the two
/// totals differ by exactly the calls that never ran.
///
/// A refused call is one kind of call that never ran. It is counted apart
/// from ok and err because a decision against a call is not a verdict on an
/// execution: the user declined or a hook blocked it, and neither says the
/// tool failed. The remaining not-run calls — interrupted, unknown, skipped
/// behind a failed edit — stay in calls alone, so the counts add up to calls
/// only when those are added back.
pub(crate) struct ToolOutcomeCounts {
    pub calls: u32,
    pub ok: u32,
    pub err: u32,
    pub refused: u32,
}

pub(crate) fn count_tool_outcomes(results: &[(String, serde_json::Value)]) -> ToolOutcomeCounts {
    let (mut calls, mut ok, mut err, mut refused) = (0u32, 0u32, 0u32, 0u32);
    for (_id, output) in results {
        calls += 1;
        // A refusal is also a call that never ran, so it is read first: a
        // refused call that fell through to the not-run branch would drop
        // out of every count instead of its own.
        if tool_result_refused(output) {
            refused += 1;
            continue;
        }
        // A call the runner answered without executing it — the turn ended
        // while it was pending, the tool is gone, an edit it follows did not
        // complete — is neither outcome.
        if tool_result_not_run(output) {
            continue;
        }
        if observability::tool_failure_reason(output).is_none() {
            ok += 1;
        } else {
            err += 1;
        }
    }
    ToolOutcomeCounts {
        calls,
        ok,
        err,
        refused,
    }
}

#[cfg(test)]
mod tests;
