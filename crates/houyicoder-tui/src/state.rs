//! Application state shared by rendering, key routing, and session updates.
//!
//! App is the composition shell; cohesive concerns own their state and
//! transitions in dedicated types rather than adding parallel fields here.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::time::Instant;

pub(crate) mod chain;
pub(crate) mod counts;
mod cursors;
pub(crate) mod enums;
pub(crate) mod history_read;
pub(crate) mod index_build;
pub(crate) mod index_read;
mod model_picker;
mod parked_view_states;
mod scroll;
mod search_view;
pub(crate) mod stage;
mod teammate_view;
mod trajectory;
pub mod transcript;

use crate::agent_message::FleetState;
#[cfg(test)]
use crate::composition::app as test_app;
use crate::display_rows_cache::DisplayRowsCache;
use crate::last_row_keys::LastRowKeys;
use crate::layout_feedback::LayoutFeedback;
use crate::layout_snapshot::LayoutSnapshot;
use crate::memory_state::MemoryPaneState;
use crate::render_cache::RenderCache;
use crate::run_state::{RunProgress, RunState};
use crate::sessions::Sessions;
use crate::ui::Ui;
use crate::workflow::Workflow;
use houyicoder_protocol::envelope::RequestId;

pub(crate) use crate::state::cursors::EventCursor;
pub use crate::state::model_picker::{
    DEFAULT_LABEL, ModelDraft, ModelPickerState, ModelSettingFocus, PendingCommit,
};
pub(crate) use crate::state::parked_view_states::ParkedViewStates;
pub use crate::state::trajectory::{
    TrajectoryDrill, TrajectoryPaneState, TrajectorySelection, TrajectoryTurnKey,
};
use ratatui::layout::Rect;

/// Live progress for one long-running tool call (bash): elapsed seconds +
/// the running stdout line count (None when the backend does not stream
/// stdout, so the chip shows "(Ns)" not "(Ns · M lines)"). Updated per
/// ToolProgress tick; cleared when the result lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BashProgress {
    pub elapsed_secs: u64,
    pub lines: Option<u64>,
}

pub use crate::artifact::{
    Annotation, AppliedChange, ArtifactMode, ArtifactSession, ChangeProposer, ProposedChange,
    StubProposer, TuiError,
};
pub use crate::evidence::{
    AuditEntry, ConsoleTodo, DiffData, Divergence, GraphResult, Hunk, HunkEvidence, MemoryEntry,
    PlanArtifact, ReviewFinding, SpecArtifact, SpecClause, Verdict, VerifyResult, audit_entry,
};
pub use crate::records::{Approval, SpecContext, StatusStub, TranscriptLine};

pub use crate::state::enums::*;

/// Hit-test geometry for the queued-input interface. The selection cursor
/// lives on the InputQueue itself (its boundary owns both the items and the
/// index into them); this struct only carries the last-rendered footer strip
/// rectangle the pointer handler reads back.
#[derive(Default)]
pub struct QueueViewState {
    /// Last-rendered queued-input footer strip rectangle.
    pub strip_rect: Cell<Rect>,
}

#[derive(Default, Debug)]
pub(crate) struct CurrentTurnBoundary {
    pub(crate) line_index: usize,
}

/// Top-level state composed from session wiring and domain-owned UI state.
pub struct App {
    /// The frontend's presentation and interaction state.
    pub ui: Ui,
    /// The spec-plan-review-verify-artifact lifecycle fields, bundled as one
    /// owner toward the final shell.
    pub(crate) workflow: Workflow,
    /// The pre-rendered display-row cache: the rows, the four per-row key
    /// vectors, the pre-rendered lines, and the version stamp that guards
    /// them together. Pub(crate) so the draw pass reads and writes a vector
    /// directly, matching the per-vector visible_window slices.
    pub(crate) display_rows_cache: DisplayRowsCache,
    pub status: StatusStub,
    pub(crate) memory: MemoryPaneState,
    pub quit: bool,
    // --- real agent-loop wiring ---
    /// The presented session and the resumable-session catalog, held as one
    /// value so a swap replaces both and a lost connection never drops the
    /// identity.
    pub(crate) sessions: Sessions,
    /// The renderer layout feedback: the last terminal height and transcript
    /// pane width the draw pass measured, held in one owner so the count and
    /// layout paths read the same dims the last render used.
    pub(crate) layout_feedback: LayoutFeedback,
    /// The renderer's layout snapshot: per-region rectangles and rendered-row
    /// caches the draw pass stashes, read back by the input and click paths.
    pub(crate) layout_snapshot: LayoutSnapshot,
    /// Per-line render cache (content hash + width + expand key; indices are
    /// unstable — transcript rebuilds each batch). Count + render share it.
    pub render_cache: RefCell<RenderCache>,
    /// The per-row keys of the last emitted visible row set: the call id,
    /// fold-group key, expanded-group mark, and turn id each visible row
    /// carries, parallel to the rows. Pub(crate) so the click and Ctrl+O
    /// handlers and the draw pass reach a vector directly.
    pub(crate) last_row_keys: LastRowKeys,
    /// Footer fleet state: the child snapshots + the Shift-arrow selection.
    pub fleet: FleetState,
    /// The /model picker: host snapshot, draft, and the commit in flight.
    pub model_picker: ModelPickerState,
}

impl App {
    /// The working transcript, borrowed for read paths and black-box tests.
    /// The conversation lives on the session; App exposes one read handle so
    /// callers do not reach through the session value.
    pub fn transcript(&self) -> &transcript::Transcript {
        &self.sessions.current.transcript
    }

    /// Public read seam for the session-owned returned-delegation rows.
    /// In-crate render and key paths read the session field directly;
    /// external integration tests reach the rows through this handle instead of
    /// the crate-private sessions field.
    pub fn agents(&self) -> &crate::agent_message::PaneAgents {
        &self.sessions.current.agents
    }

    /// Public mut seam for the session-owned returned-delegation rows: external
    /// integration tests stage fixtures through it.
    pub fn agents_mut(&mut self) -> &mut crate::agent_message::PaneAgents {
        &mut self.sessions.current.agents
    }

    /// Rebuild the returned-delegation rows from the session transcript when
    /// its revision moved, then pull the pane cursor back inside the rebuilt
    /// rows so an emptied or shortened list never leaves it pointing past the
    /// end. The session call borrows the transcript and the agents as disjoint
    /// fields inside one step; this method is also the seam external
    /// integration tests trigger a rebuild through.
    pub fn refresh_agents(&mut self) {
        self.sessions.current.refresh_agents();
        let rows = self.sessions.current.agents.rows.len();
        self.ui.agents_sel = self.ui.agents_sel.min(rows.saturating_sub(1));
    }

    /// Move the /agents pane cursor by a signed delta, clamped to the
    /// session's returned-delegation rows and a no-op while they are empty.
    /// The cursor lives with the Ui pane state; the rows it indexes are the
    /// session's, so one call reads the rows and steps the cursor.
    pub fn move_agents_cursor(&mut self, delta: i32) {
        let rows = self.sessions.current.agents.rows.len();
        if rows == 0 {
            return;
        }
        self.ui.agents_sel = (self.ui.agents_sel as i32 + delta).clamp(0, rows as i32 - 1) as usize;
    }

    /// The working transcript, mutably borrowed for fixture setup and the
    /// rebuild paths that inject resident frames. The write handle matches the
    /// read handle so test fixtures reach the conversation the same way the
    /// read assertions do instead of threading the session value.
    #[cfg(test)]
    pub(crate) fn transcript_mut(&mut self) -> &mut transcript::Transcript {
        &mut self.sessions.current.transcript
    }

    /// True while a run is in flight and not paused for approval. Excludes
    /// Waiting (the spinner stops while a card is up).
    pub fn agent_busy(&self) -> bool {
        matches!(
            self.sessions.current.run,
            RunState::Running(_) | RunState::Cancelling(_)
        )
    }

    /// The wall-clock start of the active run, or None when idle.
    pub fn run_started(&self) -> Option<Instant> {
        self.sessions.current.run.started_at()
    }

    /// True only while the user cancelled and the run is resolving.
    pub fn cancelling(&self) -> bool {
        self.sessions.current.run.is_cancelling()
    }

    /// Whether the session holds a live engine connection handle, so the
    /// send paths can enqueue commands. The handle survives a driver that
    /// dropped under ConnectionStatus::Lost, so this reports the handle's
    /// presence, not the driver's health.
    pub fn has_connection(&self) -> bool {
        self.sessions.current.connection.is_some()
    }

    /// The request id of the active run, or None when idle.
    pub fn active_run_req_id(&self) -> Option<RequestId> {
        self.sessions.current.run.request_id()
    }

    /// Borrow the streaming progress of the active run, or None when idle.
    /// Render paths read through this; the run-scoped preview does not exist
    /// outside a run.
    pub(crate) fn run_progress(&self) -> Option<&RunProgress> {
        self.sessions.current.run.progress()
    }

    /// Mutably borrow the streaming progress, or None when idle. Write paths
    /// during a run go through this; callers that cannot be idle take it and
    /// expect an active run.
    pub(crate) fn run_progress_mut(&mut self) -> Option<&mut RunProgress> {
        self.sessions.current.run.progress_mut()
    }

    /// Test seam: transition to Running with a specific request id.
    /// Replaces the old direct agent_busy write that bypassed the state
    /// machine.
    #[cfg(test)]
    pub fn start_run_for_test(&mut self, req_id: u64) {
        self.sessions
            .current
            .run
            .start(RequestId(req_id), Instant::now());
        // A running agent flips the tail fold group to active; recompute the
        // cache here because no rebuild follows a lines-built transcript.
        let groups = crate::fold::compute_fold_groups(
            self.sessions.current.transcript.lines(),
            self.agent_busy(),
        );
        *self.sessions.current.transcript.fold_groups_mut() = groups;
    }
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("screen", &self.ui.navigation.screen)
            .field("stage", &self.workflow.stage)
            .field("pane", &self.ui.navigation.pane)
            .field("viewport", &self.ui.navigation.viewport)
            .field("transcript_len", &self.sessions.current.transcript.len())
            .field("agent_busy", &self.agent_busy())
            .field("prompt", &self.sessions.current.prompt)
            .field("quit", &self.quit)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_palette_nav_no_panic() {
        let mut app = test_app();
        app.open_palette();
        app.palette_up();
        app.palette_down();
        app.palette_push('a');
        app.palette_pop();
    }

    #[test]
    fn test_console_focus_nav() {
        let mut app = test_app();
        app.console_focus_up();
        app.console_focus_down();
    }

    #[test]
    fn test_app_debug_format() {
        let app = test_app();
        drop(format!("{app:?}"));
    }

    #[test]
    fn test_has_connection_reports_presence() {
        let mut app = test_app();
        assert!(!app.has_connection());
        let (_tx, rx) = std::sync::mpsc::channel::<crate::agent_message::SessionMessage>();
        app.sessions.current.connection =
            Some(crate::session::SessionConnection::from_receiver(rx));
        assert!(app.has_connection());
    }

    #[test]
    fn test_stage_label_nonempty() {
        for s in [
            Stage::Idle,
            Stage::Design,
            Stage::Implementing,
            Stage::Verify,
            Stage::Done,
        ] {
            assert!(!s.label().is_empty());
        }
    }
}
