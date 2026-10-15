//! Session subsystem root. SessionConnection is the live engine
//! connection: it owns the command channel, message receiver, request-id
//! counter, and driver task. Session is the presented session, pairing the
//! stable identity with that optional live connection.

pub(crate) mod agent_dispatch;
pub mod agent_message;
pub(crate) mod approval;
pub(crate) mod ask_question_model;
pub(crate) mod model_receipt;
pub mod pending_prompt;
pub(crate) mod run_control;
pub(crate) mod run_metrics;
pub(crate) mod run_state;
pub(crate) mod sessions;
pub(crate) mod todo_view;

use std::cell::Cell;

use crate::agent_message::PaneAgents;
use crate::pending_prompt::PendingPrompt;
use crate::run_metrics::RunMetrics;
use crate::run_state::RunState;
use crate::session::capabilities::SessionCapabilities;
use crate::session::identity::SessionIdentity;
use crate::state::history_read::HistoryReads;
use crate::state::index_build::IndexBuild;
use crate::state::transcript::Transcript;
use crate::todo_view::TodoState;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::run::ApprovalRequest;
use houyicoder_protocol::frontend::trust::TrustPrompt;

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
    /// The sandbox the session runs under, as a display label. Client-local:
    /// the runner does not report it, so the composition root writes it where
    /// the session is wired ("mac-seatbelt"), the local login writes its own,
    /// and a bare session is "off".
    pub(crate) sandbox: String,
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
    /// Whether the most recent completed run ended FinalOutput (a clean end).
    /// Gates the queued-message auto-send: a clean end drains FIFO, an
    /// interrupt or error parks the pending item for explicit re-send. A
    /// switch carries the pre-switch value because a swap runs at idle after
    /// a clean end, so the carried value is the true state, not a forged run
    /// result.
    pub(crate) last_run_final: bool,
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
    /// status, hook, tool, skill, and memory caches plus the client-local
    /// state each capability keeps. They travel together as one session value
    /// and reset on a switch, so a new session starts with an empty cache
    /// instead of inheriting the previous session's server view.
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
    /// The returned-delegation rows derived from the transcript. They reset
    /// with the session because they are rebuilt from the session transcript;
    /// the pane's cursor lives with the Ui pane interaction state alongside
    /// its peers.
    pub(crate) agents: PaneAgents,
    /// The agent directory listing the last /agents query fetched, kept so
    /// the pane can render it before any delegation has returned. It is
    /// session-scoped server read state, reset on a switch.
    pub(crate) agent_directory: Option<String>,
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
            sandbox: "off".to_string(),
            run: RunState::Idle,
            run_metrics: RunMetrics::default(),
            last_run_input: None,
            last_run_final: false,
            transcript: Transcript::default(),
            prompt: None,
            todos: TodoState::default(),
            capabilities: SessionCapabilities::default(),
            history_reads: HistoryReads::default(),
            index_build: IndexBuild::default(),
            loaded_from_frame: Cell::new(usize::MAX),
            agents: PaneAgents::default(),
            agent_directory: None,
        }
    }

    /// Raise a permission ask and pause the run for the verdict. The run
    /// enters Waiting so its clock pauses, and the prompt holds the ask the
    /// verdict will resolve. The two move together: a permission ask without
    /// a paused run, or a paused run with no tracked ask, would leave the
    /// verdict unable to resume the right run. Callers that mutate the card
    /// or question after raising do so through the prompt accessor, after
    /// this method has committed both halves of the transition.
    pub(crate) fn raise_permission(&mut self, req_id: RequestId, requests: Vec<ApprovalRequest>) {
        self.run.begin_waiting();
        self.prompt = Some(PendingPrompt::permission(req_id, requests));
    }

    /// Clear a permission ask and resume the run. The verdict reached the
    /// driver, so the run leaves Waiting preserving its original start time.
    /// Only call this against a permission ask; a trust ask has no paused run
    /// to resume, and a run finishing on its own clears with clear_prompt so
    /// the run state is left to the finish transition.
    pub(crate) fn resolve_permission(&mut self) {
        self.prompt = None;
        self.run.end_waiting();
    }

    /// Raise the startup trust ask. No run is in flight at startup, so the
    /// run state is untouched; the prompt's presence gates new sends until
    /// answered.
    pub(crate) fn raise_trust(&mut self, req_id: RequestId, prompt: TrustPrompt) {
        self.prompt = Some(PendingPrompt::trust_ask(req_id, prompt));
    }

    /// Clear whatever prompt is up, with no run transition. Used when the run
    /// is finishing on its own (completion or cancel) or when the connection
    /// is gone so no verdict can be sent. The run state is left to the path
    /// that owns the finish or the loss.
    pub(crate) fn clear_prompt(&mut self) {
        self.prompt = None;
    }

    /// Rebuild the returned-delegation rows from the session transcript when
    /// its revision moved. Borrows the transcript and the agents as disjoint
    /// fields of the session, so the rebuild and the version check see one
    /// consistent transcript without handing the caller a split borrow.
    pub(crate) fn refresh_agents(&mut self) {
        let version = self.transcript.revision();
        let lines = self.transcript.lines();
        self.agents.refresh(lines, version);
    }
}
