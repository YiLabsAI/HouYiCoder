//! Session subsystem root. SessionConnection is the live engine
//! connection: it owns the command channel, message receiver, request-id
//! counter, and driver task. Session is the presented session, pairing the
//! stable identity with that optional live connection.

use crate::run_metrics::RunMetrics;
use crate::run_state::RunState;
use crate::session::identity::SessionIdentity;
use crate::todo_view::TodoState;

pub(crate) mod capabilities;
mod connection;
pub(crate) mod identity;
pub(crate) mod persistence;

pub use connection::{
    ConnectionStatus, EnqueueError, PollOutcome, RequestIdExhausted, SessionConnection,
};

/// The presented session: its stable identity, the optional live engine
/// connection, the in-flight run lifecycle, the run tally, the restore
/// stash, and the checklist. The identity is always present. The
/// connection is a capability the session holds while connected and may
/// lose without losing the identity or the already-observed state; the
/// loss is carried inside SessionConnection as ConnectionStatus::Lost
/// rather than by dropping the session. The remaining fields share the
/// session's lifetime and reset together on a session switch, not on a
/// run's return to idle.
pub(crate) struct Session {
    /// The session's routing id and the starting working directory, stable
    /// for the session's lifetime.
    pub(crate) identity: SessionIdentity,
    /// The live connection, present while connected and None in the bare
    /// or not-yet-connected path.
    pub(crate) connection: Option<SessionConnection>,
    /// The run lifecycle state machine: idle, running, waiting, or
    /// cancelling. The session owns it so a run resolves on the session
    /// that started it rather than on a detached shell field.
    pub(crate) run: RunState,
    /// The session's cumulative run tally: when its first run started and
    /// the tokens and steps accrued across completed runs.
    pub(crate) run_metrics: RunMetrics,
    /// The submitted input of the run that produced no output yet, kept so
    /// an interruption can restore the turn.
    pub(crate) last_run_input: Option<String>,
    /// The session checklist: derived items, expansion, and completion
    /// visibility.
    pub(crate) todos: TodoState,
}

impl Session {
    /// A session with an identity, no connection, no run in flight, an
    /// empty tally, and an empty checklist: the state the default app lands
    /// in before the composition root attaches a connection.
    pub(crate) fn bare(identity: SessionIdentity) -> Self {
        Self {
            identity,
            connection: None,
            run: RunState::Idle,
            run_metrics: RunMetrics::default(),
            last_run_input: None,
            todos: TodoState::default(),
        }
    }
}
