//! Count tool outcomes (calls / ok / err) for a batch of results. Split
//! from call.rs so that file stays under the size gate.

use crate::observability;
use houyicoder_protocol::tool::tool_result_not_run;

/// Named counts for a batch of tool outcomes. A tuple (calls, ok, err)
/// would let a return-order swap compile silently; the named struct makes
/// the producer's field assignment unambiguous.
///
/// A call answered without executing it still counts in calls, because this
/// tally answers what the model asked for. The per-tool observability tally
/// answers what each tool did and drops such calls entirely, so the two
/// totals differ by exactly the calls that never ran.
pub(crate) struct ToolOutcomeCounts {
    pub calls: u32,
    pub ok: u32,
    pub err: u32,
}

pub(crate) fn count_tool_outcomes(results: &[(String, serde_json::Value)]) -> ToolOutcomeCounts {
    let (mut calls, mut ok, mut err) = (0u32, 0u32, 0u32);
    for (_id, output) in results {
        calls += 1;
        // A call the runner answered without executing it — the turn ended
        // while it was pending, the user rejected it — is neither outcome.
        if tool_result_not_run(output) {
            continue;
        }
        if observability::tool_failure_reason(output).is_none() {
            ok += 1;
        } else {
            err += 1;
        }
    }
    ToolOutcomeCounts { calls, ok, err }
}

#[cfg(test)]
mod tests;
