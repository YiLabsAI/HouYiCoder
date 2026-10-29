//! The one classification of a run's terminal state: the durable record and
//! the completion rate both read it, so a branch that names the wrong state
//! would inflate the rate with no other symptom.

use super::*;

/// Every terminal outcome names its own state. A swapped branch here would
/// count a failed or handed-off run as completed and nothing else would
/// notice.
#[test]
fn test_completion_status_per_outcome() {
    let cases = [
        (
            RunOutcome::FinalOutput("done".into()),
            RunCompletionStatus::Completed,
        ),
        (
            RunOutcome::Interruption(Vec::new()),
            RunCompletionStatus::Interrupted,
        ),
        (
            RunOutcome::Handoff(AgentId("child".into())),
            RunCompletionStatus::HandedOff,
        ),
        (
            RunOutcome::Interrupted("user cancelled".into()),
            RunCompletionStatus::Interrupted,
        ),
        (
            RunOutcome::VerifyFailed(VerifyFailure::new()),
            RunCompletionStatus::VerificationFailed,
        ),
        (
            RunOutcome::MaxTurnsReached { turns: 3 },
            RunCompletionStatus::TurnLimitReached,
        ),
    ];
    for (outcome, expected) in cases {
        assert_eq!(
            outcome.completion_status(),
            expected,
            "the state of {outcome:?}"
        );
    }
}

/// The summary string a watcher reads belongs to the same outcome: the state
/// and the text cannot disagree about what happened.
#[test]
fn test_terminal_status_keeps_summary() {
    assert_eq!(
        RunOutcome::FinalOutput("the answer".into()).terminal_status(),
        (RunCompletionStatus::Completed, "the answer".to_string())
    );
    assert_eq!(
        RunOutcome::Interrupted("user cancelled".into()).terminal_status(),
        (
            RunCompletionStatus::Interrupted,
            "user cancelled".to_string()
        )
    );
    assert_eq!(
        RunOutcome::MaxTurnsReached { turns: 1 }.terminal_status(),
        (RunCompletionStatus::TurnLimitReached, String::new())
    );
}
