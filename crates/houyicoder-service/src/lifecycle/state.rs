//! The lifecycle state machine: the closed state set and the failures a
//! transition can report.
//!
//! The set is closed rather than non_exhaustive because adding a state forces
//! every runtime match to handle it; that is a deliberate breaking change, not
//! an additive one.

use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;

/// The lifecycle states a session moves through. A closed, stable set: adding a
/// state is a deliberate breaking change (every runtime match arm must handle
/// it), so the enum is not non_exhaustive. Shutdown and Cancelled are terminal.
///
/// Transition shape (this trait documents it; a later runtime step enforces it):
/// Startup -> Running on init; Running <-> Checkpointed on compaction resume;
/// Running -> PendingPermission when a tool asks; PendingPermission -> Running
/// on reply or -> Cancelled on a reap, or -> Detached on disconnect (pending
/// retained); Detached -> PendingPermission on reconnect WITH pending (the ask
/// is re-sent, still awaiting reply) or -> Running without pending (lease
/// auto-take or observer); Running -> Cancelled on abort; Running -> Shutdown
/// on Handoff (the spawn target is a NEW SessionRecord's Startup, not an edge
/// into this machine) or on session end; Detached -> Shutdown on stop or lease
/// expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    Startup,
    Running,
    Checkpointed,
    PendingPermission,
    Detached,
    /// Terminal: the session handed off (spawn target is a new session) or ended.
    Shutdown,
    /// Terminal: the session was aborted (RunCancel or a pending-ask reap).
    Cancelled,
}

/// Failures a lifecycle operation can report.
#[derive(Debug)]
pub enum LifecycleError {
    /// No session matched the id.
    NotFound,
    /// The session is not in a state the operation allows.
    InvalidTransition(String),
    /// Another client holds the control lease and force was not requested.
    LeaseHeld(String),
    /// An underlying store or I/O failure.
    Io,
}

impl Display for LifecycleError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("session not found"),
            Self::InvalidTransition(msg) => write!(f, "invalid lifecycle transition: {msg}"),
            Self::LeaseHeld(msg) => write!(f, "control lease held by another client: {msg}"),
            Self::Io => f.write_str("lifecycle store I/O failure"),
        }
    }
}

impl Error for LifecycleError {}
