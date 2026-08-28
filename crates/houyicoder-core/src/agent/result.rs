//! Run outcome and error types. Pure data types extracted from the runner
//! module so the runner file stays under the file-size gate. The runner
//! re-exports these at the agent module root so callers name them as
//! agent::RunResult etc.

use houyicoder_context::ContextError;
use houyicoder_protocol::llm::{ProviderError, Usage};

use super::step::ApprovalRequest;
use super::verify::VerifyFailure;
use houyicoder_context::AgentId;

/// How a run ended.
#[derive(Debug)]
pub enum RunOutcome {
    /// The model produced a final answer.
    FinalOutput(String),
    /// A tool needs human approval before executing. Call resume with the
    /// caller's decisions to continue.
    Interruption(Vec<ApprovalRequest>),
    /// The turn handed off to another agent. the caller spawns it.
    Handoff(AgentId),
    /// An external abort cancelled an in-flight run. The partial assistant
    /// text was flushed to the log and any tool calls left without a result
    /// received a synthetic interrupted-by-user error result, so the session
    /// stays lossless and resumable. The string carries the reason.
    Interrupted(String),
    /// A run reached FinalOutput but the verify gate rejected it — the run
    /// output failed make check (or adversarial verify). The caller surfaces
    /// the findings and re-prompts the model to fix its own work. Only fires
    /// when a verify gate is installed; with no gate, FinalOutput passes
    /// through unchanged.
    VerifyFailed(VerifyFailure),
    /// The run hit the max_turns backstop: the model kept calling tools
    /// without producing a final answer. Not a crash — the run is
    /// resumable, and the result still carries the turns and usage
    /// accumulated so far. A graceful max-turns result (is_error semantics
    /// with cost/usage statistics) rather than a throw.
    MaxTurnsReached { turns: u32 },
}

impl RunOutcome {
    /// Coarse terminal status + a summary string for a watcher that does
    /// not read the session log. The status names match the spawn runtime's
    /// terminal_summary; the summary is the FinalOutput text (or empty for
    /// non-text terminals — the precise partial lives in the log).
    pub fn terminal_status(&self) -> (&'static str, String) {
        match self {
            Self::FinalOutput(t) => ("completed", t.clone()),
            Self::MaxTurnsReached { .. } => ("max_turns", String::new()),
            Self::Interrupted(s) => ("interrupted", s.clone()),
            Self::Interruption(_) => ("interrupted", String::new()),
            Self::VerifyFailed(_) => ("verify_failed", String::new()),
            Self::Handoff(a) => ("handoff", a.0.clone()),
        }
    }
}

/// The result of a run.
#[derive(Debug)]
pub struct RunResult {
    pub outcome: RunOutcome,
    pub turns: u32,
    pub usage: Usage,
}

/// A run failure. Provider errors split into fatal (non-retryable, first
/// attempt) and exhausted (retried out); context errors cover the store.
/// Overflow errors are fail-closed (non-fatal) — the handler compresses and
/// retries, but bounded to prevent infinite retry avalanche.
#[derive(Debug)]
pub enum RunError {
    Context(ContextError),
    ProviderFatal(ProviderError),
    ProviderExhausted(ProviderError),
    /// A forked sub-run (memory extraction) hit its max_turns backstop
    /// without finishing. Unlike the main loop's graceful
    /// RunOutcome::MaxTurnsReached, a fork that does not finish is a
    /// failure so the extractor reconsiders the range. Fork-path only.
    MaxTurnsExceeded {
        turns: u32,
    },
    /// The served view exceeded the context window after bounded compress
    /// retries. Fail-closed: the session is NOT bricked (the raw log is
    /// intact), but the loop stops to avoid hammering the provider with
    /// oversized requests. The caller surfaces a user choice (rewind to
    /// checkpoint / more aggressive summary).
    ///
    /// When the overflow came from a provider ContextOverflow error, the
    /// enforced_limit is the real window the provider named (already
    /// recorded for the catalog via record_learned_context_window). It is
    /// None on the pre-flight path where the served view exceeded the
    /// threshold before a request was sent. Surfacing it lets the caller
    /// tell the user the real limit rather than only the retry count.
    ContextOverflowBounded {
        retries: u32,
        enforced_limit: Option<u32>,
    },
    /// Compress made no progress (all events Verbatim, nothing to fold).
    /// Fail-closed: further compress attempts would not shrink the window.
    /// The caller surfaces a user choice.
    ContextOverflowNoProgress,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Context(e) => write!(f, "context error: {e}"),
            Self::ProviderFatal(e) => write!(f, "provider fatal: {e}"),
            Self::ProviderExhausted(e) => write!(f, "provider exhausted: {e}"),
            Self::MaxTurnsExceeded { turns } => {
                write!(f, "fork hit max turns limit after {turns} turns")
            }
            Self::ContextOverflowBounded {
                retries,
                enforced_limit,
            } => match enforced_limit {
                Some(limit) => write!(
                    f,
                    "context overflow after {retries} compress retries (provider enforces {limit} tokens; fail-closed)"
                ),
                None => write!(
                    f,
                    "context overflow after {retries} compress retries (fail-closed)"
                ),
            },
            Self::ContextOverflowNoProgress => {
                write!(
                    f,
                    "context overflow: compress made no progress (fail-closed)"
                )
            }
        }
    }
}

impl std::error::Error for RunError {}

impl From<ContextError> for RunError {
    fn from(e: ContextError) -> Self {
        Self::Context(e)
    }
}
