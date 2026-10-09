//! Application state shared by rendering, key routing, and session updates.
//!
//! App is the composition shell; cohesive concerns own their state and
//! transitions in dedicated types rather than adding parallel fields here.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

pub(crate) mod chain;
pub(crate) mod counts;
mod cursors;
pub(crate) mod enums;
pub(crate) mod history_read;
pub(crate) mod index_read;
mod model_picker;
mod parked_view_states;
mod scroll;
mod search_view;
pub(crate) mod stage;
mod teammate_view;
mod trajectory;
pub(crate) mod transcript;

use crate::agent_message::{FleetState, PaneAgents};
use crate::composition::WorktreeEntry;
#[cfg(test)]
use crate::composition::app as test_app;
use crate::console_state::ConsoleState;
use crate::display_rows_cache::DisplayRowsCache;
use crate::history::HistoryNav;
use crate::hooks_state::HooksPaneState;
use crate::input::InputField;
use crate::last_row_keys::LastRowKeys;
use crate::layout_feedback::LayoutFeedback;
use crate::layout_snapshot::LayoutSnapshot;
use crate::list_pane_state::ListPaneState;
use crate::memory_state::MemoryPaneState;
use crate::notifications::NotificationState;
use crate::palette::PaletteState;
use crate::paste::{PasteStore, PendingPaste};
use crate::pending_prompt::PendingPrompt;
use crate::pending_queue::PendingItem;
use crate::permission_pane_state::PermissionPaneState;
use crate::records::TeammateView;
use crate::render_cache::RenderCache;
use crate::resume_picker::{SessionCatalog, SessionPickerState};
use crate::review_queue::ReviewQueue;
use crate::run_state::{RunProgress, RunState};
use crate::selection::ClipboardWriter;
use crate::selections::Selections;
use crate::session::SessionConnection;
use crate::session::capabilities::SessionCapabilities;
use crate::skill_picker_state::SkillPickerState;
use crate::skills_state::SkillsPaneState;
use crate::todo_view::TodoState;
use crate::transcript::snapshot::TranscriptSnapshot;
use crate::view::export_log::ExportLog;
use crate::view::trajectory_pane::TrajectoryLog;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::hooks::HookEntry;

pub(crate) use crate::state::cursors::EventCursor;
pub use crate::state::model_picker::{
    DEFAULT_LABEL, ModelDraft, ModelPickerState, ModelSettingFocus, PendingCommit,
};
pub(crate) use crate::state::parked_view_states::ParkedViewStates;
pub use crate::state::trajectory::{
    TrajectoryDrill, TrajectoryPaneState, TrajectorySelection, TrajectoryTurnKey,
};
use houyicoder_protocol::frontend::skills::SkillEntry;
use houyicoder_protocol::frontend::tools::ToolEntry;
use houyicoder_protocol::frontend::{LoginMode, SessionId};
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

/// Selection and hit-test geometry for the queued-input interface.
#[derive(Default)]
pub struct QueueViewState {
    /// Selected row in the queue pane.
    pub cursor: usize,
    /// Last-rendered queued-input footer strip rectangle.
    pub strip_rect: Cell<Rect>,
}

impl QueueViewState {
    /// Clamp the selected row to the current queue length.
    pub(crate) fn clamp(&mut self, len: usize) {
        self.cursor = self.cursor.min(len.saturating_sub(1));
    }

    /// Move the selected row by a signed delta within the queue bounds.
    pub(crate) fn move_cursor(&mut self, delta: i32, len: usize) {
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let cursor = self.cursor.min(len - 1) as i32;
        self.cursor = (cursor + delta).clamp(0, (len - 1) as i32) as usize;
    }
}

#[derive(Default, Debug)]
pub(crate) struct CurrentTurnBoundary {
    pub(crate) line_index: usize,
}

/// Top-level state composed from session wiring and domain-owned UI state.
pub struct App {
    pub screen: Screen,
    pub stage: Stage,
    pub pane: Pane,
    /// The active viewport mode (Working / Focus / Scroll). Drives the layout
    /// in view::working: how many rows of chrome surround the content.
    pub viewport: ViewportMode,
    /// The viewport the user was in before entering Scroll, so Esc/End returns to it rather than re-deriving from stage (which would lose a manual Focus->Working fold).
    pub prev_viewport: ViewportMode,
    pub input: InputField,
    /// Up/Down prompt history navigation (cache + cursor + draft + abort
    /// skip-set). Backed by a JSONL file at the config home.
    pub history: HistoryNav,
    pub transcript: transcript::Transcript,
    /// The background history reads of the working transcript and the history
    /// generation each was dispatched against. Beside the transcript, not
    /// inside it: the transcript owns the resident facts, while this owner
    /// governs a read task's lifecycle across those facts being reset.
    pub history_reads: history_read::HistoryReads,
    /// Cursor into the frame log for the verdict audit cache. Stays on App:
    /// verdicts are an audit capability, not transcript content.
    pub verdict_cursor: usize,
    /// The transcript view's own state, as one value the view on screen
    /// holds: the two scroll objects and the window flag today, the search
    /// view and the expansion sets as their readers migrate in. The flat
    /// fields below stay until each group's readers move over.
    pub(crate) transcript_view: transcript::view::TranscriptViewState,
    /// The pre-rendered display-row cache: the rows, the four per-row key
    /// vectors, the pre-rendered lines, and the version stamp that guards
    /// them together. Pub(crate) so the draw pass reads and writes a vector
    /// directly, matching the per-vector visible_window slices.
    pub(crate) display_rows_cache: DisplayRowsCache,
    /// True while the G full-scan builds the event-byte-offset index (one
    /// chunk at a time keeps the UI responsive; Esc interrupts). The loop
    /// advances the build while this is set, off the draw path.
    pub indexing: Cell<bool>,
    /// Bytes of the log indexed so far (for the indexing-percent chrome),
    /// published as each chunk lands.
    pub indexed_bytes: Cell<u64>,
    /// Total log bytes the index covers (the frozen file size).
    pub index_total: Cell<u64>,
    /// True when the full index is built (event_count/byte_at answer).
    pub index_done: Cell<bool>,
    /// The index chunk dispatched and not yet applied, if any. A chunk reads
    /// the log, so the loop advances the build and the draw path never does.
    pub pending_index_chunk: std::cell::RefCell<Option<index_read::PendingIndexChunk>>,
    /// Optional full-history disk-search seam. None in stub / unwired modes
    /// (the /search --all flag then reports no disk results). When wired, the
    /// composition root injects an impl that reads the durable session log +
    /// maps SessionLogEntries to searchable text — the TUI never touches the log.
    /// Optional trajectory-data seam. The composition root injects an impl
    /// that reads the durable session log and folds events into a
    /// TrajectoryView; None in stub and unwired modes falls back to the mock
    /// trajectory so the pane still renders a demo.
    pub trajectory_log: Option<Arc<dyn TrajectoryLog>>,
    /// Optional export seam. The composition root injects an impl that reads
    /// the durable session log and serializes the full trajectory, tool
    /// stats, usage, checkpoints, and errors to a JSON document. None when
    /// the export source is not installed; /export then reports the reason
    /// (disconnected, or unavailable in this session).
    pub export_log: Option<Arc<dyn ExportLog>>,
    /// Optional transcript-snapshot seam. The composition root injects an
    /// impl that loads the durable session log into a TranscriptLine
    /// snapshot for the search view (the read-whole path for logs under
    /// the threshold). None in stub or unwired modes, where the search
    /// view falls back to the in-memory transcript vec.
    pub snapshot: Option<Arc<dyn TranscriptSnapshot>>,
    /// The session catalog for the /resume picker (resumable sessions with
    /// derived titles). None in stub/test bundles.
    pub session_catalog: Option<Arc<dyn SessionCatalog>>,
    /// The session picker overlay state (opened by /resume with no arg).
    pub resume_picker: SessionPickerState,
    /// A pending resume request set when the user picks a session in the
    /// picker (or /resume <id|name|file>). Carries a session id OR an export
    /// file path (the resume builder dispatches on which). The event loop's
    /// try_switch_session consumes it: with a resume_builder wired (the normal
    /// path), it builds the new bundle and switch_session swaps in place — no
    /// quit, no restart. Only when no builder is wired does it put the target
    /// back + set quit, letting the caller fall back to a fresh re-enter.
    pub pending_resume_target: Option<String>,
    pub palette: PaletteState,
    /// The reverse request awaiting a verdict, when one is up: a permission
    /// ask (plain approval or interactive question) during Waiting, or a
    /// startup workspace-trust ask before any run. At most one at a time.
    pub prompt: Option<PendingPrompt>,
    pub status: StatusStub,
    pub spec_ctx: SpecContext,
    pub spec_clauses: Vec<SpecClause>,
    pub diff: DiffData,
    pub spec_artifact: SpecArtifact,
    pub plan_artifact: PlanArtifact,
    pub review: ReviewQueue,
    pub console: ConsoleState,
    pub verify_result: VerifyResult,
    pub graph_result: GraphResult,
    pub(crate) memory: MemoryPaneState,
    /// Selection and hit-test state for the queued-input interface.
    pub queue_view: QueueViewState,
    /// The linked-worktree rows for the /worktrees pane. Refreshed from
    /// parse_worktrees on pane-open. Empty until the user opens the pane (no
    /// background poll — the list is cheap and the pane is one-shot).
    pub worktree_entries: Vec<WorktreeEntry>,
    /// Cursor + search query for the /worktrees pane. The first pane to
    /// adopt ListPaneState; others migrate on touch-ratchet.
    pub worktree_list: ListPaneState,
    /// /worktrees pane drill-down: 0 = list, 1 = detail.
    pub worktree_level: Cell<u8>,
    /// The /trajectory pane's drill level, cursor, and selection. One owner
    /// for the pane's position, because the selection and the cursor are kept
    /// consistent by the same transitions.
    pub trajectory: TrajectoryPaneState,
    pub agents: PaneAgents,
    pub agent_directory: Option<String>,
    /// An opened artifact for inline review and annotation. Stub content; real
    /// wiring reads the file from disk.
    pub artifact: ArtifactSession,
    /// The proposer that turns an annotation into a pending proposed edit.
    /// Concrete stub for now; the ChangeProposer trait is the seam for a real
    /// LLM-backed proposer later.
    pub proposer: StubProposer,
    pub login_mode: Option<LoginMode>,
    /// Stack of stages the chain moved through, so /rewind can pop back one.
    pub stage_history: Vec<Stage>,
    /// True while a canned replay indicator is on screen (set by /replay).
    pub replaying: bool,
    pub quit: bool,
    // --- real agent-loop wiring ---
    /// The active session id passed to the server over the wire. The TUI holds
    /// no engine handle: run, resume, and streaming all cross the wire, driven
    /// by the server task that owns the runner. None of the engine run/resume
    /// paths live here.
    pub session_id: SessionId,
    /// The tokio runtime that drives async run/resume. None while disconnected.
    pub runtime: Option<Arc<tokio::runtime::Runtime>>,
    /// The live connection with the engine: owns the command channel to the
    /// driver, the message channel back to the event loop, the request-id
    /// counter, and the driver task handle. None in the pure-stub path.
    pub session: Option<SessionConnection>,
    /// Transient notification toast: one-line auto-expiring hint above the
    /// input box (copy feedback, exit-again prompt). Poll-driven expiry.
    pub notifications: NotificationState,
    /// Whether the terminal window has focus (FocusGained/FocusLost events).
    /// The input cursor (invert) gates on this so the caret hides when the
    /// window is unfocused, following a renderPlaceholder terminal
    /// focus gate. Defaults true (assume focused at startup).
    pub terminal_focused: bool,
    /// The run lifecycle state machine: Idle, Running, Waiting, Cancelling.
    /// The single source of truth for whether a run is in flight.
    pub run_state: RunState,
    /// When the session's first run started, for end-to-end elapsed.
    pub session_started_at: Option<Instant>,
    /// Cumulative output tokens across all turns this session.
    pub cumulative_tokens: u64,
    /// Cumulative model-call steps across all turns.
    pub cumulative_steps: u32,
    /// Session checklist content, expansion, and completion lifecycle.
    pub todos: TodoState,
    /// The renderer layout feedback: the last terminal height and transcript
    /// pane width the draw pass measured, held in one owner so the count and
    /// layout paths read the same dims the last render used.
    pub(crate) layout_feedback: LayoutFeedback,
    /// Last token count displayed by the spinner. Lerps toward the actual
    /// count each frame for a smooth increment animation.
    pub displayed_tokens: Cell<u32>,
    /// The original run input while it remains eligible for no-output rollback.
    /// A committed mid-turn input or visible output closes this window.
    pub last_run_input: Option<String>,
    /// Queued user inputs submitted while a run was in flight (FIFO). A
    /// Typed queue (messages + slash commands); drained FIFO at idle.
    pub pending: Vec<PendingItem>,
    /// The four per-region drag selections: the transcript's main selection
    /// and the status, pane, and approval selections, each in its own
    /// coordinate space so a drag on one region never collides with another.
    pub(crate) selections: Selections,
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
    /// State parked for views that are not on screen: the sessions switched
    /// away from, the children stepped out of, and the parent's own sets
    /// while a child view covers it. The visible view's four open-row sets
    /// live on the transcript view state; every render and toggle path reads
    /// them without knowing which view is on screen.
    pub(crate) parked_view_states: ParkedViewStates,
    /// Drilled-in teammate transcript; when Some, active_transcript swaps to
    /// the child's turns with a banner. Enter opens, Shift+Up/Down closes;
    /// Esc interrupts the viewed child's turn.
    pub teammate_view: Option<TeammateView>,
    /// Footer fleet state: the child snapshots + the Shift-arrow selection.
    pub fleet: FleetState,
    /// Verbose render: force results, reasoning, and fold groups expanded
    /// with untruncated chips. Set in the search view, cleared on exit.
    pub verbose: bool,
    /// Side table for large pastes that were replaced by placeholder tokens
    /// in the input box; expanded back to full text on submit.
    pub pasted: PasteStore,
    /// The paste chunks accumulated before the 50ms gap flush, holding the
    /// buffer text and the last-chunk timestamp together so a flush check
    /// drains and restores both as one value.
    pub(crate) pending_paste: PendingPaste,
    /// The session's named capability composition: the wire-reported caches
    /// the panes and status paths read. One owner so those caches travel
    /// together as one value rather than a scatter of flat fields.
    pub(crate) capabilities: SessionCapabilities,
    /// The permission pane state: the selected tab, the cursor row, the
    /// typed-input sub-mode, and the pane-local search buffer. Pub(crate) so
    /// the key handler and render path reach a field directly; the whole value
    /// is what a tab switch or input-mode entry replaces.
    pub(crate) permission_pane: PermissionPaneState,
    /// The original working directory the session started in, shown at the top
    /// of the Workspace tab in /permissions (empty in a stub App).
    pub working_dir: String,
    /// The registered-hook rows for the /hooks pane. Refreshed from the wire
    /// (HooksResult) when the user opens /hooks. Empty until the first reply.
    pub hook_entries: Vec<HookEntry>,
    pub tool_entries: Vec<ToolEntry>,
    /// The discovered-skill rows for the /skills pane. Refreshed from
    /// the wire when the user opens /skills. Empty until the first
    /// reply.
    pub skill_entries: Vec<SkillEntry>,
    /// The /skills pane view state: the list cursor and the open detail
    /// (body request, body text, scroll position). Kept together so the
    /// pane's view state is one owner, not a scatter of App fields.
    pub(crate) skills_pane: SkillsPaneState,
    /// Session-scoped disabled skills (toggled via t in the detail view).
    /// Persisted disable is a follow-up (settings wire).
    pub skill_disabled: HashSet<String>,
    /// The @ skill-picker overlay state: the open flag and its list cursor.
    /// Opening resets the cursor to zero, so the two move as one value.
    pub(crate) skill_picker: SkillPickerState,
    /// The hooks pane state: the drill-down level (0 = event list, 1 = the
    /// selected event detail) and the selected event index. Pub(crate) so the
    /// key handler and render path step a field directly; the level and the
    /// selection move together on enter and return.
    pub(crate) hooks_pane: HooksPaneState,
    /// The oldest frame the scrollback has loaded, in absolute frame
    /// coordinates, or usize::MAX before any load. It is the view's requested
    /// boundary; the transcript's resident front is the memory boundary, and a
    /// load stops at it since a drained frame cannot be rendered.
    pub loaded_from_frame: Cell<usize>,
    /// The /model picker: host snapshot, draft, and the commit in flight.
    pub model_picker: ModelPickerState,
    /// The active /status sub-tab (Status / Config / Usage). Tab or Left/Right
    /// cycles it; the pane header renders the three titles with the active one
    /// highlighted. A Settings-modal-style multiple tabs.
    pub status_tab: StatusTab,
    /// In-place session-name edit buffer for the status Status tab. None
    /// unless the user pressed e on the Status tab to rename; the pane
    /// renders the name row as an editable input + the keys route char,
    /// backspace, Left/Right to the buffer. Enter commits (sends a
    /// RenameSession request), Esc cancels. Houyi makes the session name
    /// inline-editable (rather than a rename command).
    pub status_name_edit: Option<InputField>,
    pub last_title: Option<String>,
    /// True between an Esc-abort and the matching Done(Interrupted). Set in
    /// abort_run when the session/cancel notification ships; cleared in the
    /// Done handler. The honest form for a wire abort: the token fire is
    /// Pluggable clipboard writer. Production holds a SystemClipboard
    /// (pbcopy/OSC 52); adversarial selection tests inject a RecordingClipboard
    /// so the exact copied text can be asserted without touching the OS
    /// clipboard. Arc<dyn> so App stays Send + Sync across the TUI/runner.
    pub clipboard: Arc<dyn ClipboardWriter>,
}

impl App {
    /// True while a run is in flight and not paused for approval. Excludes
    /// Waiting (the spinner stops while a card is up).
    pub fn agent_busy(&self) -> bool {
        matches!(
            self.run_state,
            RunState::Running(_) | RunState::Cancelling(_)
        )
    }

    /// The wall-clock start of the active run, or None when idle.
    pub fn run_started(&self) -> Option<Instant> {
        self.run_state.started_at()
    }

    /// True only while the user cancelled and the run is resolving.
    pub fn cancelling(&self) -> bool {
        self.run_state.is_cancelling()
    }

    /// The request id of the active run, or None when idle.
    pub fn active_run_req_id(&self) -> Option<RequestId> {
        self.run_state.request_id()
    }

    /// Borrow the streaming progress of the active run, or None when idle.
    /// Render paths read through this; the run-scoped preview does not exist
    /// outside a run.
    pub(crate) fn run_progress(&self) -> Option<&RunProgress> {
        self.run_state.progress()
    }

    /// Mutably borrow the streaming progress, or None when idle. Write paths
    /// during a run go through this; callers that cannot be idle take it and
    /// expect an active run.
    pub(crate) fn run_progress_mut(&mut self) -> Option<&mut RunProgress> {
        self.run_state.progress_mut()
    }

    /// Test seam: transition to Running with a specific request id.
    /// Replaces the old direct agent_busy write that bypassed the state
    /// machine.
    #[cfg(test)]
    pub fn start_run_for_test(&mut self, req_id: u64) {
        self.run_state.start(RequestId(req_id), Instant::now());
        // A running agent flips the tail fold group to active; recompute the
        // cache here because no rebuild follows a lines-built transcript.
        let groups = crate::fold::compute_fold_groups(self.transcript.lines(), self.agent_busy());
        *self.transcript.fold_groups_mut() = groups;
    }
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("screen", &self.screen)
            .field("stage", &self.stage)
            .field("pane", &self.pane)
            .field("viewport", &self.viewport)
            .field("transcript_len", &self.transcript.len())
            .field("agent_busy", &self.agent_busy())
            .field("prompt", &self.prompt)
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
