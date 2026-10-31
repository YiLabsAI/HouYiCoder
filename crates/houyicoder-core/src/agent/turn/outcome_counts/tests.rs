//! Tests for this module.

use super::*;

fn batch(results: Vec<(&str, serde_json::Value)>) -> Vec<(String, serde_json::Value)> {
    results
        .into_iter()
        .map(|(id, output)| (id.to_string(), output))
        .collect()
}

/// A call that ran is counted with its outcome, so the ordinary split keeps
/// every result where it was.
#[test]
fn test_counts_each_outcome() {
    let counts = count_tool_outcomes(&batch(vec![
        ("c1", serde_json::json!({ "stdout": "ok", "success": true })),
        ("c2", serde_json::json!({ "error": "command not found" })),
    ]));
    assert_eq!((counts.calls, counts.ok, counts.err), (2, 1, 1));
}

/// A call the runner never executed is not evidence that the tool works, so
/// it stays out of the success count rather than inflating it.
#[test]
fn test_unexecuted_call_uncounted() {
    let counts = count_tool_outcomes(&batch(vec![
        ("c1", serde_json::json!({ "stdout": "ok", "success": true })),
        (
            "c2",
            serde_json::json!({ "error": "interrupted by user", "not_run": true }),
        ),
    ]));
    assert_eq!(counts.calls, 2, "the call still happened");
    assert_eq!(counts.ok, 1, "only the call that ran counts as success");
    assert_eq!(counts.err, 0, "a call that did not run did not fail");
}
