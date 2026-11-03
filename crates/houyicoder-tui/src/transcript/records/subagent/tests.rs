use super::*;

/// The trace names the task, not the agent type, when the delegation carried
/// a summary; a child whose start was never observed shows no duration rather
/// than a zero-length one, and a single tool reads singular.
#[test]
fn test_trace_text_names_task() {
    let full = SubagentFinished {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        description: "find the auth module".into(),
        outcome: SubagentOutcome::Finished,
        duration_ms: Some(12_000),
        tool_uses: 7,
    }
    .text();
    assert_eq!(
        full,
        "\u{25cf} Agent \"find the auth module\" finished \u{00b7} 12s \u{00b7} 7 tools"
    );
    let stripped = SubagentFinished {
        child_sid: "c2".into(),
        subagent_type: "explore".into(),
        description: String::new(),
        outcome: SubagentOutcome::Interrupted,
        duration_ms: None,
        tool_uses: 1,
    }
    .text();
    assert_eq!(
        stripped,
        "\u{25cf} Agent \"explore\" interrupted \u{00b7} 1 tool"
    );
    assert!(
        !stripped.contains("0s"),
        "no duration is claimed: {stripped}"
    );
}

/// A status that carried no tool figure is not reported as a measured zero:
/// the segment is left out, and the ending and the duration still read.
#[test]
fn test_trace_omits_zero_tools() {
    let text = SubagentFinished {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        description: "find the auth module".into(),
        outcome: SubagentOutcome::Finished,
        duration_ms: Some(12_000),
        tool_uses: 0,
    }
    .text();
    assert_eq!(
        text,
        "\u{25cf} Agent \"find the auth module\" finished \u{00b7} 12s"
    );
    assert!(!text.contains("tool"), "no count is claimed: {text}");
}

/// An invisible direction mark in the model-written summary cannot reorder
/// the row: it is replaced before the text reaches the screen, so the row
/// reads left to right as the text was given.
#[test]
fn test_trace_strips_hidden_marks() {
    let text = SubagentFinished {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        description: "find\u{202e}auth".into(),
        outcome: SubagentOutcome::Finished,
        duration_ms: None,
        tool_uses: 0,
    }
    .text();
    assert!(
        !text.contains('\u{202e}'),
        "the direction mark never reaches the row: {text:?}"
    );
    assert!(
        text.contains("find auth"),
        "the text reads as given: {text:?}"
    );
}

/// Each terminal status maps to the ending the row names. An ending this
/// build does not know still ended the child, but the row says only that:
/// reading it as success would claim an ending nobody observed.
#[test]
fn test_outcome_names_each_status() {
    assert_eq!(
        SubagentOutcome::from_completion(CompletionStatus::Completed),
        SubagentOutcome::Finished
    );
    assert_eq!(
        SubagentOutcome::from_completion(CompletionStatus::Failed),
        SubagentOutcome::Failed
    );
    assert_eq!(
        SubagentOutcome::from_completion(CompletionStatus::Killed),
        SubagentOutcome::Interrupted
    );
    assert_eq!(
        SubagentOutcome::from_completion(CompletionStatus::TimedOut),
        SubagentOutcome::TimedOut
    );
    assert_eq!(
        SubagentOutcome::from_completion(CompletionStatus::TurnLimit),
        SubagentOutcome::TurnLimit
    );
    assert_eq!(
        SubagentOutcome::from_completion(CompletionStatus::BudgetExhausted),
        SubagentOutcome::Budget
    );
    assert_eq!(
        SubagentOutcome::from_completion(CompletionStatus::Unknown("some_future_word".into())),
        SubagentOutcome::Unknown
    );
    assert_eq!(SubagentOutcome::Unknown.label(), "ended");
    assert_eq!(SubagentOutcome::TimedOut.label(), "timed out");
    assert_eq!(SubagentOutcome::TurnLimit.label(), "finished (turn limit)");
    assert_eq!(SubagentOutcome::Budget.label(), "finished (budget)");
}

/// A run long enough to reach the next unit reads in that unit, the same
/// step the session summary header uses: a minute of work states one minute
/// rather than sixty seconds.
#[test]
fn test_trace_duration_steps_up() {
    let text = SubagentFinished {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        description: "find the auth module".into(),
        outcome: SubagentOutcome::Finished,
        duration_ms: Some(60_000),
        tool_uses: 7,
    }
    .text();
    assert_eq!(
        text,
        "\u{25cf} Agent \"find the auth module\" finished \u{00b7} 1m \u{00b7} 7 tools"
    );
}
