//! What a hook returns: the immutable context it reads, the pure verdict it
//! produces, the structured error when it cannot produce one, and the
//! host-side combination of a batch of results into one effective verdict.

use houyicoder_context::SessionId;

use super::event::{HookEvent, HookPayload};
use super::wire::verdict_on_hook_error;

// HookContext — immutable evaluation context.

/// Immutable context passed to a hook at evaluation time. The hook reads
/// event, payload, and session, then returns a pure verdict. Trust and
/// capability are NOT passed to the hook: trust filtering is centralized
/// in the registry dispatch (the host skips untrusted hooks before
/// evaluation), and capability scoping lands with the WASM executor. This
/// keeps the context minimal and avoids premature fields with no
/// consumers.
///
/// Owned (no lifetime): the parallel dispatch path wraps the context in an
/// Arc and passes it to detached threads. A hook that exceeds its timeout
/// is abandoned — its thread leaks but the context remains valid because
/// the data is owned, not borrowed.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: HookEvent,
    pub payload: HookPayload,
    pub session: SessionId,
}

// HookVerdict — the 7 pure verdicts.

/// Pure verdict returned by a hook. The host (agent loop) executes flow
/// control based on this value; the hook trait stays execution-agnostic.
#[derive(Debug, Clone)]
pub enum HookVerdict {
    /// No objection; the host proceeds.
    Allow,
    /// Security hard stop. Synchronous block; terminal (no retry).
    Deny(String),
    /// Quality self-correction signal. The host injects it into the next
    /// model prompt for in-loop rewriting with bounded retry.
    Feedback(String),
    /// Non-blocking observation. The host records it to the observability
    /// log. The string carries the observation note so the host can show
    /// it without re-deriving context.
    Observe(String),
    /// Inject content into the flowing event (memory recall, redaction,
    /// additional context).
    Inject(String),
    /// Escalate to the user (human-in-the-loop). The host blocks until the
    /// user responds.
    Ask(String),
    /// Fire a downstream event (for example, a large PostToolUse output
    /// triggers a compress). Asynchronous; does not block the current
    /// verdict combination.
    Trigger(HookEvent),
}

// HookError — structured, never crashes the host.

/// Structured hook error. All variants are logged to the session store and
/// shown in the trajectory view. A hook error never panics the host.
#[derive(Debug, Clone)]
pub enum HookError {
    /// WASM guest trapped (panic or unreachable instruction).
    GuestPanic {
        hook_name: String,
        backtrace: String,
    },
    /// Hook exceeded its time budget. Bounded: default 5s for mechanical
    /// rules, 60s for agent-type verification.
    Timeout { hook_name: String, limit_ms: u64 },
    /// Hook returned an invalid verdict (schema violation).
    InvalidVerdict { hook_name: String, detail: String },
    /// Hook tried to access an undeclared host function.
    CapabilityDenied {
        hook_name: String,
        capability: String,
    },
    /// Feedback retry exhausted (N attempts, still violating the rule).
    FeedbackExhausted { hook_name: String, attempts: u8 },
    /// Malformed hook configuration.
    ConfigError { detail: String },
    /// External-process hook communication failure.
    ProcessError { hook_name: String, reason: String },
}

// CombinedVerdict — composite of blocking + non-blocking signals.

/// Composite verdict produced by combine_verdicts. The primary field carries the
/// blocking verdict (the one the host acts on first). Observations and
/// triggers are non-blocking side signals collected from ALL hooks in the
/// batch — they are never dropped even when a blocking verdict is present.
/// This is the core advantage over a single-verdict return: the host can
/// short-circuit on Deny while still recording observations and firing
/// triggers that accompanied the deny.
#[derive(Debug, Clone)]
pub struct CombinedVerdict {
    /// The effective blocking verdict, by priority: Deny, Ask, Feedback,
    /// Inject, Trigger (first), Observe (joined), Allow. The host matches
    /// on this for flow control.
    pub primary: HookVerdict,
    /// Non-blocking observation notes. Always collected, even when primary
    /// is Deny/Ask/Feedback/Inject. CONTROL-FLOW RESIDUAL ONLY — the
    /// durable record goes through the dispatch-point HookSignal (one per
    /// hook, with hook_name); this joined Vec exists for the host's own
    /// use and is not the audit record.
    pub observations: Vec<String>,
    /// All trigger events collected from the batch. The host fires these
    /// asynchronously once the batch is combined. When triggers are the sole signal,
    /// the primary also carries the first trigger for backward compatibility.
    pub triggers: Vec<HookEvent>,
}

// combine_verdicts — deny-wins composite verdict (host-side).

/// Combine a batch of hook results into one verdict. The primary
/// field carries the blocking verdict by priority: Deny, Ask, Feedback,
/// Inject, Trigger (first), Observe (joined), Allow. Observations and
/// triggers are ALWAYS collected from every hook regardless of whether a
/// blocking verdict is present — the host short-circuits on Deny while
/// still recording observations + firing triggers that accompanied it.
/// Errors are fail-closed via verdict_on_hook_error (single source).
pub fn combine_verdicts(
    results: impl IntoIterator<Item = Result<HookVerdict, HookError>>,
) -> CombinedVerdict {
    let mut deny: Option<String> = None;
    let mut ask: Option<String> = None;
    let mut feedback: Vec<String> = Vec::new();
    let mut inject: Vec<String> = Vec::new();
    let mut triggers: Vec<HookEvent> = Vec::new();
    let mut observations: Vec<String> = Vec::new();

    for result in results {
        match result {
            Ok(HookVerdict::Allow) => {}
            Ok(HookVerdict::Deny(reason)) => {
                if deny.is_none() {
                    deny = Some(reason);
                }
            }
            Ok(HookVerdict::Feedback(signal)) => feedback.push(signal),
            Ok(HookVerdict::Observe(note)) => observations.push(note),
            Ok(HookVerdict::Inject(content)) => inject.push(content),
            Ok(HookVerdict::Ask(question)) => {
                if ask.is_none() {
                    ask = Some(question);
                }
            }
            Ok(HookVerdict::Trigger(ev)) => triggers.push(ev),
            // Fail-closed: the policy lives in verdict_on_hook_error so the
            // durable HookSignal recorder writes the same effective verdict
            // and the two can never diverge. If this becomes fail-open, one
            // edit changes both sides.
            Err(_) => {
                if let HookVerdict::Deny(reason) = verdict_on_hook_error()
                    && deny.is_none()
                {
                    deny = Some(reason);
                }
            }
        }
    }

    // Primary by priority: deny, ask, feedback, inject, trigger, observe, allow.
    // Observations and triggers are returned alongside regardless of primary.
    let primary = if let Some(reason) = deny {
        HookVerdict::Deny(reason)
    } else if let Some(question) = ask {
        HookVerdict::Ask(question)
    } else if !feedback.is_empty() {
        HookVerdict::Feedback(feedback.join("; "))
    } else if !inject.is_empty() {
        HookVerdict::Inject(inject.join("\n"))
    } else if let Some(first) = triggers.first() {
        HookVerdict::Trigger(*first)
    } else if !observations.is_empty() {
        HookVerdict::Observe(observations.join("; "))
    } else {
        HookVerdict::Allow
    };

    CombinedVerdict {
        primary,
        observations,
        triggers,
    }
}
