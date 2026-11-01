//! The durable session record and the parked turn it carries.
//!
//! These are the cross-process serde shapes: a record written by one process
//! is read back verbatim by the next one that reattaches, so every field is
//! either a shared protocol type or owned bytes.

use houyicoder_context::{EventId, SessionId};
use houyicoder_protocol::frontend::permission::AskReason;
use houyicoder_protocol::frontend::run::ApprovalDecision;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The durable, session-indexed companion to a live runner. Fields are pub so
/// the runtime layer reads and updates them; construction goes through the
/// composition root, never external crates.
///
/// pending is indexed by session, not by connection: a client that detaches
/// while a permission ask is in flight leaves the ask here, and whichever client
/// reattaches via load_session receives the re-sent ask. This is the single
/// source of truth for pending-permission state, so the lifecycle layer and the
/// control-lease spec never hold two divergent copies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The session this record tracks.
    pub session_id: SessionId,
    /// The last event id appended to the session log (the resume cursor).
    pub event_cursor: EventId,
    /// The half-live turn retained across detach + reconnect: the unanswered
    /// asks plus the verdicts already received. None when no turn is parked at
    /// an interruption. Indexed by session (not connection) so a reattaching
    /// client re-receives the remaining asks and the runner resumes with the
    /// full decided set. Carries the whole turn, not a single ask, so a
    /// mid-batch disconnect does not lose the verdicts already given (which
    /// would otherwise die with the connection-local Vec and could not be
    /// re-asked without double-appending the audit chain).
    pub pending: Option<PendingTurn>,
    /// An opaque runner checkpoint the runtime can resume from (the serialized
    /// runner state at the last safe resume point). Owned bytes so the lifecycle
    /// layer does not depend on a runner-internal type.
    pub runner_checkpoint: Vec<u8>,
    /// The current control lease holder (a client or connection identifier), or
    /// None when the session is detached with no live holder.
    pub lease_holder: Option<String>,
}

/// A pending permission ask retained across detach + reconnect. Indexed by the
/// session, not by the connection that originated it, so the re-sent ask reaches
/// whichever client reattaches. Matches the reverse-request payload so the
/// runtime can re-send it verbatim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingPermission {
    /// The provider-minted tool-call id the verdict answers.
    pub call_id: String,
    /// The tool name the ask is about.
    pub tool: String,
    /// The tool input the ask carries (may be edited by the human).
    pub input: Value,
    /// Why the call needs a verdict, as the gate reported it when the call was
    /// judged. Retained so the re-emitted card explains the call with the cause
    /// that produced the ask; a record written before this field existed
    /// deserializes to None, and the card falls back to a generic prompt.
    #[serde(default)]
    pub reason: Option<AskReason>,
}

/// The full half-live turn parked at an interruption: the asks still awaiting a
/// verdict (in the order the server emits them, head first) plus the verdicts
/// already received. A disconnect mid-batch keeps both — the remaining asks are
/// re-emitted on reattach, and the runner resumes with the full decided set
/// (already-given verdicts plus the ones the reattaching client supplies).
///
/// Both fields are wire types so the record stays the cross-process serde
/// boundary; the single-approval common case degenerates to a one-element
/// remaining list and an empty decided list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingTurn {
    /// Unanswered asks still to emit (or re-emit on reconnect), head first.
    pub remaining: Vec<PendingPermission>,
    /// Verdicts already received this turn, in the order received. Fed to
    /// runner.resume together with the freshly-received ones once remaining
    /// drains.
    pub decided: Vec<ApprovalDecision>,
}
