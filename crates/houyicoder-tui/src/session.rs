//! Session subsystem root. SessionConnection is the live engine
//! connection: it owns the command channel, message receiver, request-id
//! counter, and driver task. Session is the presented session, pairing the
//! stable identity with that optional live connection.

use crate::run_metrics::RunMetrics;
use crate::run_state::RunState;
use crate::session::identity::SessionIdentity;

pub(crate) mod capabilities;
mod connection;
pub(crate) mod identity;
pub(crate) mod persistence;

pub use connection::{
    ConnectionStatus, EnqueueError, PollOutcome, RequestIdExhausted, SessionConnection,
};

/// The presented session: its stable identity, the optional live engine
/// connection, the in-flight run lifecycle, and the run tally that
/// outlives any one run. The identity is always present. The connection is
/// a capability the session holds while connected and may lose without
/// losing the identity or the already-observed state; the loss is carried
/// inside SessionConnection as ConnectionStatus::Lost rather than by
/// dropping the session. The tally spans the whole session while resetting
/// on a session switch, so it does not belong to the run state machine.
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
}

impl Session {
    /// A session with an identity, no connection, no run in flight, and an
    /// empty tally: the state the default app lands in before the
    /// composition root attaches a connection.
    pub(crate) fn bare(identity: SessionIdentity) -> Self {
        Self {
            identity,
            connection: None,
            run: RunState::Idle,
            run_metrics: RunMetrics::default(),
            last_run_input: None,
        }
    }
}
