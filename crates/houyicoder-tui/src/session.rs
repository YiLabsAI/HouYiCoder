//! Session subsystem root. SessionConnection is the live engine
//! connection: it owns the command channel, message receiver, request-id
//! counter, and driver task. Session is the presented session, pairing the
//! stable identity with that optional live connection.

use std::cell::Cell;

use crate::pending_prompt::PendingPrompt;
use crate::run_metrics::RunMetrics;
use crate::run_state::RunState;
use crate::session::capabilities::SessionCapabilities;
use crate::session::identity::SessionIdentity;
use crate::state::history_read::HistoryReads;
use crate::state::index_build::IndexBuild;
use crate::state::transcript::Transcript;
use crate::todo_view::TodoState;

pub(crate) mod capabilities;
mod connection;
pub(crate) mod identity;
pub(crate) mod persistence;

pub use connection::{
    ConnectionStatus, EnqueueError, PollOutcome, RequestIdExhausted, SessionConnection,
};

/// The presented session: its stable identity, the optional live engine
/// connection, the in-flight run lifecycle, the run tally, the pending
/// reverse ask, the restore stash, the working transcript, the named
/// capabilities, and the transcript read state. The identity is always
/// present. The connection is a capability the session holds while
/// connected and may lose without losing the identity or the
/// already-observed state; the loss is carried inside SessionConnection
/// as ConnectionStatus::Lost rather than by dropping the session. The
/// remaining fields share the session's lifetime and reset together on a
/// session switch, not on a run's return to idle.
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
    /// The working transcript: the resident frame window and the lines,
    /// blocks, and fold groups derived from it. The conversation belongs to
    /// the session, so a switch rebuilds it empty instead of carrying the
    /// previous session's frames across.
    pub(crate) transcript: Transcript,
    /// The pending reverse ask awaiting a human verdict, when one is up: a
    /// permission ask (approval or question) while waiting, or a startup
    /// trust ask before any run.
    pub(crate) prompt: Option<PendingPrompt>,
    /// The session checklist: derived items, expansion, and completion
    /// visibility.
    pub(crate) todos: TodoState,
    /// The session's named capabilities: the server-reported permission,
    /// status, hook, tool, and skill caches plus the client-local state each
    /// capability keeps. They travel together as one session value and reset
    /// on a switch, so a new session starts with an empty cache instead of
    /// inheriting the previous session's server view.
    pub(crate) capabilities: SessionCapabilities,
    /// The background reads of the working transcript and the history
    /// generation each was dispatched against. Beside the transcript, not
    /// inside it: the transcript owns the resident facts, while this owner
    /// governs a read task's lifecycle across those facts being reset.
    pub(crate) history_reads: HistoryReads,
    /// The full transcript scan's index progress mirror and pending chunk,
    /// held in one owner.
    pub(crate) index_build: IndexBuild,
    /// The oldest frame the scrollback has loaded, in absolute frame
    /// coordinates, or usize::MAX before any load. It is the view's
    /// requested boundary; the transcript's resident front is the memory
    /// boundary, and a load stops at it since a drained frame cannot be
    /// rendered.
    pub(crate) loaded_from_frame: Cell<usize>,
}

impl Session {
    /// A session with an identity, no connection, no run in flight, an
    /// empty tally, no pending ask, an empty checklist, empty capability
    /// caches, and an unread transcript: the state the default app lands
    /// in before the composition root attaches a connection.
    pub(crate) fn bare(identity: SessionIdentity) -> Self {
        Self {
            identity,
            connection: None,
            run: RunState::Idle,
            run_metrics: RunMetrics::default(),
            last_run_input: None,
            transcript: Transcript::default(),
            prompt: None,
            todos: TodoState::default(),
            capabilities: SessionCapabilities::default(),
            history_reads: HistoryReads::default(),
            index_build: IndexBuild::default(),
            loaded_from_frame: Cell::new(usize::MAX),
        }
    }
}
