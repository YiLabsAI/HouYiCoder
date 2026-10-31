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

/// A provider failure reads as the provider's own sentence. Naming the
/// failure class in front of it would add a second colon and push the cause
/// the user can act on to the end of the line; the wire category already
/// carries the class, so the text does not repeat it.
#[test]
fn test_provider_failure_display() {
    assert_eq!(
        RunError::ProviderFatal(ProviderError::Network).to_string(),
        "the connection to the model provider dropped",
    );
    assert_eq!(
        RunError::ProviderExhausted(ProviderError::Network).to_string(),
        "the connection to the model provider dropped (retries exhausted)",
    );
    // The class is not named in the text for either variant.
    for err in [
        RunError::ProviderFatal(ProviderError::Auth),
        RunError::ProviderExhausted(ProviderError::Auth),
    ] {
        let rendered = err.to_string();
        assert!(
            !rendered.contains("provider fatal") && !rendered.contains("provider exhausted:"),
            "the class prefix is gone: {rendered}"
        );
    }
    // Every other variant still names its own condition.
    assert!(
        RunError::MaxTurnsExceeded { turns: 3 }
            .to_string()
            .contains("3 turns")
    );
    assert!(
        RunError::ContextOverflowBounded {
            retries: 2,
            enforced_limit: Some(128_000),
        }
        .to_string()
        .contains("128000")
    );
    assert!(
        RunError::ContextOverflowNoProgress
            .to_string()
            .contains("no progress")
    );
}
