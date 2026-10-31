//! The classifications a logged record states about an outcome: how a run
//! ended, how a human answered a permission request, and why a turn's output
//! was cut. They live beside the event vocabulary because the log, the run
//! observer events, and the panes must report one taxonomy rather than three
//! interpretations of the same records.

use serde::{Deserialize, Serialize};

/// How a run ended, as its terminal record states it. A finished run and a
/// cancelled one both write a terminal record, so the record alone cannot say
/// whether the work was done; this is what lets a completion rate count a
/// cancelled run as cancelled rather than as work that finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunCompletionStatus {
    /// The run produced its final output.
    Completed,
    /// The run transferred control to another agent.
    HandedOff,
    /// The run was cancelled while it was running.
    Interrupted,
    /// The run failed before producing final output.
    Failed,
    /// Post-run verification rejected the output.
    VerificationFailed,
    /// The run exhausted its configured turn limit.
    TurnLimitReached,
}

/// The outcome of a human permission verdict on a tool call. The durable
/// companion to the transient approval popup: every approve / deny decision is
/// appended to the session log so the audit trail is replayable and queryable
/// (logging verdicts only in-memory would leave an audit gap — a denied
/// destructive call would leave no durable trace; appending to the log
/// closes this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionVerdict {
    Approved,
    Denied,
}

impl PermissionVerdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

/// Which silent-truncation signal fired for a turn. The drive loop computes
/// this once from the folded turn state so it both synthesizes the cap-cut
/// finish_reason and records the cause in the verdict, without re-running
/// the heuristic. Stays None when the provider signaled the cut directly —
/// the raw finish_reason already carries the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TruncationSignal {
    /// Server-reported output_tokens reached the max cap (within slack).
    ServerUsageNearCap,
    /// Self-counted output tokens (the shared Tokenizer) reached the max cap
    /// (within slack). Fires when the server did not report usage — common
    /// for streaming proxies that omit the usage block.
    SelfCountNearCap,
    /// An odd triple-backtick count means a code fence opened but never
    /// closed — the cut landed mid-block.
    UnclosedCodeBlock,
    /// No silent-truncation signal fired.
    None,
}
