//! Application state shared by rendering, key routing, and session updates.
//!
//! App is the composition shell; cohesive concerns own their state and
//! transitions in dedicated types rather than adding parallel fields here.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, mpsc};
use std::time::Instant;

pub(crate) mod app_methods;
pub(crate) mod counts;
mod cursors;
pub(crate) mod enums;
mod expanded_keys;
pub(crate) mod history_read;
mod model_picker;
mod scroll;
mod search_view;
mod teammate_view;
pub(crate) mod transcript;

use crate::agent_message::{FleetState, PaneAgents, SessionMessage};
use crate::composition::WorktreeEntry;
#[cfg(test)]
use crate::composition::app as test_app;
use crate::console_state::ConsoleState;
use crate::history::HistoryNav;
use crate::input::InputField;
use crate::list_pane_state::ListPaneState;
use crate::memory_state::MemoryPaneState;
use crate::notifications::NotificationState;
use crate::palette::PaletteState;
use crate::paste::PasteStore;
use crate::pending_prompt::PendingPrompt;
use crate::pending_queue::PendingItem;
use crate::records::{TeammateView, ToolOutcome};
use crate::render_cache::RenderCache;
use crate::resume_picker::{SessionCatalog, SessionPickerState};
use crate::review_queue::ReviewQueue;
use crate::run_state::{RunProgress, RunState};
use crate::scroll::{SearchState, TranscriptScroll, WindowScroll};
use crate::selection::{ClipboardWriter, Selection};
use crate::session::SessionConnection;
use crate::todo_view::TodoState;
use crate::transcript::snapshot::TranscriptSnapshot;
use crate::view::export_log::ExportLog;
use crate::view::trajectory_pane::TrajectoryLog;
use houyicoder_protocol::acp_wire::PermissionOptionKind;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::context::ContextBreakdown;
use houyicoder_protocol::frontend::hooks::HookEntry;

pub(crate) use crate::state::cursors::EventCursor;
pub(crate) use crate::state::expanded_keys::ParkedKeys;
pub use crate::state::model_picker::{
    DEFAULT_LABEL, ModelDraft, ModelPickerState, ModelSettingFocus, PendingCommit,
};
use houyicoder_protocol::frontend::permission::{
    PermissionDecisionEntry, PermissionMode, PermissionRule,
};
use houyicoder_protocol::frontend::skills::SkillEntry;
use houyicoder_protocol::frontend::status::StatusSnapshot;
use houyicoder_protocol::frontend::tools::ToolEntry;
use houyicoder_protocol::frontend::{LoginMode, SessionId};
use ratatui::layout::Rect;
use ratatui::text::Line;

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
    /// Cursor into the frame log for the verdict audit cache. Stays on App:
    /// verdicts are an audit capability, not transcript content.
    pub verdict_cursor: usize,
    /// Cursor anchored on the frame the user last saw before scrolling away
    /// from the tail, for the new-message count. Absolute (Local) or durable
    /// (Server) so front-of-window eviction and resume cannot silently shift
    /// the baseline the way a raw frame index would.
    pub(crate) unseen_since: Option<EventCursor>,
    pub transcript_scroll: TranscriptScroll,
    /// Cached display rows: the full pre-visible computation (display_slots +
    /// row formatting). Invalidated by a version counter — only recomputed
    /// when the transcript or display inputs change, not every frame.
    pub display_rows_cache: RefCell<Vec<(u8, String, Option<ToolOutcome>)>>,
    pub display_rows_version: Cell<u64>,
    pub cached_callids: RefCell<Vec<Option<String>>>,
    pub cached_fold_keys: RefCell<Vec<Option<String>>>,
    pub cached_expanded_group: RefCell<Vec<Option<String>>>,
    pub cached_turn_ids: RefCell<Vec<Option<String>>>,
    pub cached_pre_rendered: RefCell<Vec<Option<Line<'static>>>>,
    pub search: SearchState,
    /// Frozen snapshot the search view renders + counts against. Empty
    /// outside the search view; active_transcript picks it when search.active
    /// so count + render + highlight read one source.
    pub search_transcript: Vec<TranscriptLine>,
    /// True when the snapshot seam declined to load the whole log (log over
    /// the threshold) and search_transcript is empty as an honest degrade.
    /// The status bar shows a "log too large" hint instead of "no match".
    pub search_truncated: bool,
    /// The raw log byte size at search-view enter, for the degrade hint
    /// ("log is N MB"). Zero when no snapshot seam is wired.
    pub snapshot_log_bytes: u64,
    /// Corrupt log lines the tolerant read skipped at enter. The status bar
    /// surfaces "N lines skipped" so the user sees data was dropped (not a
    /// silent gap). Zero on the strict-replay path (replay errors instead).
    pub search_skipped: usize,
    /// True when the search view is in byte-window mode (log over the
    /// threshold). Renders flat (no fold slots) through a separate path that
    /// does not touch TranscriptScroll/display_slots/total: the slot layer
    /// (fold grouping + collapse handles) has no meaning when one screen is
    /// materialized at a time. Row-layer rendering is shared with the live
    /// path.
    pub window_mode: bool,
    /// The byte offset where the loaded window starts. For the byte-%
    /// position indicator (divided by frozen_file_size).
    pub window_anchor: u64,
    /// The byte offset past the loaded window's last line (== file size for
    /// the tail window). Scrolling newer loads a window starting here.
    pub window_end: u64,
    /// The log byte size frozen at search-view enter. Window reads stay in
    /// [0, frozen) so events appended after enter are invisible (I6 snapshot
    /// consistency -- the window does not chase the growing tail).
    pub frozen_file_size: u64,
    /// Within-window row scroll state. Separate from TranscriptScroll (the
    /// whole-vec path) so the 5 total consumers stay on their own path.
    pub window_scroll: WindowScroll,
    /// Corrupt lines skipped in the current window (separate from the
    /// whole-log search_skipped so window-mode chrome shows the per-window
    /// count).
    pub window_skipped: usize,
    /// True while the G full-scan builds the event-byte-offset index across
    /// frames (one chunk per frame keeps the UI responsive; Esc interrupts).
    /// The flat render path drives index_chunk while this is set. Cell so the
    /// draw borrow (&App) can flip it off when the build completes.
    pub indexing: Cell<bool>,
    /// Bytes of the log indexed so far (for the indexing-percent chrome),
    /// published by the render path each frame while indexing.
    pub indexed_bytes: Cell<u64>,
    /// Total log bytes the index covers (the frozen file size).
    pub index_total: Cell<u64>,
    /// True when the full index is built (event_count/byte_at answer).
    pub index_done: Cell<bool>,
    /// Optional full-history disk-search seam. None in stub / unwired modes
    /// (the /search --all flag then reports no disk results). When wired, the
    /// composition root injects an impl that reads the durable session log +
    /// projects SessionLogEntries to searchable text — the TUI never touches the log.
    /// Optional trajectory-data seam. The composition root injects an impl
    /// that reads the durable session log and projects events into a
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
    /// /trajectory pane drill-down state: 0 = turn list, 1 = turn detail
    /// (events + ASCII bar), 2 = event detail (full data).
    pub trajectory_level: Cell<u8>,
    /// Cursor into the current level's list (turn list at level 0, event
    /// list at level 1). Clamped to the list length at render time.
    pub trajectory_cursor: Cell<usize>,
    /// List length at the current drill level, stashed by the render path so
    /// the Up/Down key handler can clamp the cursor in [0, len-1] — without
    /// this the cursor grows past the last row on Down and the selection
    /// glyph vanishes (no row matches the out-of-range index).
    pub trajectory_list_len: Cell<usize>,
    /// The L0-selected row index, frozen on drill so L1/L2 render the row
    /// the user picked (not always the first turn — drilling a later turn or
    /// a [bg] row showed the first turn's events before this field existed).
    pub trajectory_turn_idx: Cell<usize>,
    /// True when the L0 row is a bg event (skips L2 drill-in).
    pub trajectory_at_bg: Cell<bool>,
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
    /// Sender cloned into each spawned task; the task ships the RunResult plus
    /// the session replay back over this channel.
    pub agent_tx: Option<mpsc::Sender<SessionMessage>>,
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
    /// Last terminal height seen by the draw pass, stashed for height-aware
    /// checklist rendering. Interior-mutable for draw-borrow updates.
    pub last_terminal_rows: Cell<u16>,
    /// Last transcript PANE width (inner area.width, not the input last_cols).
    /// Stashed so the count path soft-wraps to the same width render used
    /// (count == render). 0 before first render = do-not-wrap.
    pub last_transcript_width: Cell<u16>,
    /// Last token count displayed by the spinner. Lerps toward the actual
    /// count each frame for a smooth increment animation.
    pub displayed_tokens: Cell<u32>,
    /// The original run input while it remains eligible for no-output rollback.
    /// A committed mid-turn input or visible output closes this window.
    pub last_run_input: Option<String>,
    /// Queued user inputs submitted while a run was in flight (FIFO). A
    /// Typed queue (messages + slash commands); drained FIFO at idle.
    pub pending: Vec<PendingItem>,
    /// In-app text selection (drag-select in the transcript, copy on release).
    pub selection: Selection,
    /// Last-rendered transcript rect (screen coords), stashed by the draw
    /// pass so the mouse handler can map a click cell to a transcript row.
    pub transcript_rect: Cell<Rect>,
    /// Last-rendered jump-to-bottom label rect; hit-tested before the
    /// transcript surface. Zero rect when hidden.
    pub jump_to_bottom_rect: Cell<Rect>,
    /// Last-rendered transcript rows with their style tag (post-wrap, with
    /// spacer blanks), stashed by the draw pass so copy can extract the
    /// selected text and skip non-content rows (spinner).
    pub last_transcript_rows: RefCell<Vec<(u8, String)>>,
    /// Full transcript rows (pre-slice) stashed by the draw pass so copy can
    /// access content beyond the visible viewport (selection past the bottom
    /// edge, or viewport scrolled between draw and copy).
    pub last_all_rows: RefCell<Vec<(u8, String)>>,
    /// Last-rendered slash-command pane rect (the /permissions /search
    /// /memory inner content region), stashed by the draw pass so the mouse
    /// handler can route a drag in the pane to a pane-local selection. Zero
    /// when no command pane is open.
    pub pane_rect: Cell<Rect>,
    /// Last-rendered pane content rows, stashed by reading the frame buffer
    /// after the pane content closure draws. The panes render through
    /// arbitrary widgets (List, Paragraph, SearchBox), so the rendered cells
    /// are the single source of truth for the text the user sees — reading
    /// them avoids duplicating each widget's row construction.
    pub last_pane_rows: RefCell<Vec<(u8, String)>>,
    /// Last-rendered status bar rect, published per viewport by the draw pass;
    /// zeroed at view::draw top so it cannot go stale across viewports or
    /// screens.
    pub status_rect: Cell<Rect>,
    /// Status bar rows read back from the frame buffer (like last_pane_rows)
    /// so copy extracts the model/mode/context text the user sees.
    pub last_status_rows: RefCell<Vec<(u8, String)>>,
    /// Selection for the status bar surface; own coordinate space so it never
    /// collides with the transcript or a pane.
    pub status_selection: Selection,
    /// In-app selection for the slash-command pane surface (separate from the
    /// transcript selection so the two coordinate spaces never collide). A
    /// drag in the pane starts here; mouse-up copies the pane text and clears
    /// the range, since pane content rebuilds each frame and a persistent
    /// highlight would not track the rows.
    pub pane_selection: Selection,
    /// The approval card's screen rect (zero when no approval is shown),
    /// stashed by the draw pass so the mouse handler routes a drag in the
    /// card to a card-local selection.
    pub approval_rect: Cell<Rect>,
    /// Last-rendered approval card rows, read from the frame buffer like
    /// last_pane_rows so copy extracts the text the user sees.
    pub last_approval_rows: RefCell<Vec<(u8, String)>>,
    /// Selection for the approval card surface (separate coordinate space).
    pub approval_selection: Selection,
    /// Per-line render cache (content hash + width + expand key; indices are
    /// unstable — transcript rebuilds each batch). Count + render share it.
    pub render_cache: RefCell<RenderCache>,
    /// Parallel to last_transcript_rows: the result call_id a visible row
    /// belongs to (Some on a summary row) so Ctrl+O maps the anchor's row to
    /// the result to toggle, without changing the (u8, String) copy tuple.
    pub last_row_callids: RefCell<Vec<Option<String>>>,
    /// Result call_ids the user has expanded (Ctrl+O) so the full multi-line
    /// body shows. Keyed by call_id (not row index) so expansion survives the
    /// wholesale transcript rebuild on each event batch.
    pub expanded_results: HashSet<String>,
    /// Fold-group keys the user has expanded (Ctrl+O or click on a summary).
    /// Keyed by the group's first tool call_id so the choice survives rebuilds.
    /// Active-turn groups are always expanded and never enter this set.
    pub expanded_fold_groups: HashSet<String>,
    /// Per-ThoughtFor-line expansion state, keyed by that line's turn_id.
    /// Empty = collapsed.
    pub expanded_thinking: HashSet<String>,
    pub expanded_subagents: HashSet<String>,
    /// Expansion sets parked by the session that owns them, so a switch away
    /// and back restores what the user had open.
    pub(crate) parked_keys: ParkedKeys,
    /// Drilled-in teammate transcript; when Some, active_transcript swaps to
    /// the child's turns with a banner. Enter opens, Shift+Up/Down closes;
    /// Esc interrupts the viewed child's turn.
    pub teammate_view: Option<TeammateView>,
    /// Footer fleet state: the child snapshots + the Shift-arrow selection.
    pub fleet: FleetState,
    /// Verbose render: force results, reasoning, and fold groups expanded
    /// with untruncated chips. Set in the search view, cleared on exit.
    pub verbose: bool,
    /// Parallel to last_row_callids: the fold-group key a visible row belongs
    /// to (Some on a collapsed summary / expanded collapse-hint row) so Ctrl+O
    /// and click can toggle the fold group under the selection anchor.
    pub last_row_fold_keys: RefCell<Vec<Option<String>>>,
    /// Parallel to last_row_fold_keys but marks ONLY rows inside an EXPANDED
    /// fold group (the expanded summary header + every body row the group
    /// emits). Consumed by the click-release handler (click anywhere in an
    /// expanded block collapses it) and the render path (gray bg on the
    /// expanded block). Kept separate from last_row_fold_keys so Ctrl+O's
    /// result-first contract stays intact: Ctrl+O reads fold_keys (None on
    /// body rows) and expands the result under the cursor, never collapses the
    /// group from a body row.
    pub last_row_expanded_group: RefCell<Vec<Option<String>>>,
    /// Parallel to last_row_fold_keys: the ThoughtFor turn_id a visible row
    /// carries (Some on a "Thought for Ns" header row, None elsewhere) so the
    /// click handler maps a click's row straight to the turn_id to toggle,
    /// without counting Nth-visible-then-Nth-in-full-transcript (which
    /// misaligned when ThoughtFor rows scrolled out of the viewport — the
    /// visible count skipped off-screen rows while the full-transcript count
    /// did not, so clicking a visible row toggled an off-screen turn).
    pub last_row_turn_ids: RefCell<Vec<Option<String>>>,
    /// Side table for large pastes that were replaced by placeholder tokens
    /// in the input box; expanded back to full text on submit.
    pub pasted: PasteStore,
    /// Accumulates bracketed-paste chunks that arrive in multiple Paste
    /// events (large pastes are chunked by the terminal). Flushed (ingested)
    /// after a 50ms gap with no new chunk.
    pub paste_buffer: Option<String>,
    /// Timestamp of the last paste chunk, for the gap-based flush.
    pub paste_last: Option<Instant>,
    /// Last computed input wrap column count (set by the draw pass, read by
    /// key handlers for cursor up/down in wrapped space). Interior-mutable so
    /// the draw borrow of App can update it without going through &mut.
    pub last_cols: Cell<usize>,
    /// Hidden hardware cursor position used by terminal input methods.
    pub native_cursor_position: Cell<Option<(u16, u16)>>,
    /// The permission mode cache, wire-typed. Seeded once on the first idle
    /// poll (so the status-bar pill renders from session start), then updated
    /// by the PermissionMode / PermissionCycleMode responses the server ships
    /// on Shift+Tab cycle. The server is the single write authority for mode;
    /// the TUI never imports the permission crate's gate.
    pub mode_cache: Option<PermissionMode>,
    /// Durable rule cache (wire-typed), refreshed by PermissionRulesResult.
    pub rules_cache: Vec<PermissionRule>,
    pub dirs_cache: Vec<String>,
    /// Ask-before-git checkpoint toggle cache (default on); /permission git refreshes it.
    pub ask_before_git_enabled: bool,
    /// /context cache: last breakdown, rendered immediately on /context (refreshed in background). None until the first ContextResult.
    pub context_cache: Option<ContextBreakdown>,
    /// Per-tool last-used verdict (identity, not list position). The cursor
    /// preselect reads this so rejecting bash lands on No next time.
    /// Session-scoped; not persisted across processes.
    pub sticky_choices: HashMap<String, PermissionOptionKind>,
    /// The session verdict log, from the acpx/context/permission_decision stream — the client-side audit trail of every approve/deny.
    pub verdict_log_cache: Vec<PermissionDecisionEntry>,
    /// Selected tab in /permission (Allow/Ask/Deny filter rules; Recent shows the verdict log).
    pub permission_tab: PermissionTab,
    /// Cursor row in the current /permission tab; clamped at render.
    pub permission_cursor: usize,
    /// Active typed-input sub-mode in /permission (add/remove/search); None =
    /// list navigation, re-purposing the main input box.
    pub permission_input: PermissionInput,
    /// Pane-local search buffer for the /permissions SearchBox. Decoupled from
    /// the main input so search never eats a slash command's leading slash.
    pub permission_search: String,
    /// The original working directory the session started in, shown at the top
    /// of the Workspace tab in /permissions (empty in a stub App).
    pub working_dir: String,
    /// The latest protocol status snapshot. None while disconnected.
    pub status_cache: Option<StatusSnapshot>,
    /// When the last periodic StatusQuery was sent. Fires every
    /// STATUS_POLL_INTERVAL_SECS so the bar + /sandbox stay recent.
    pub last_status_poll: Option<Instant>,
    /// The registered-hook rows for the /hooks pane. Refreshed from the wire
    /// (HooksResult) when the user opens /hooks. Empty until the first reply.
    pub hook_entries: Vec<HookEntry>,
    pub tool_entries: Vec<ToolEntry>,
    /// The discovered-skill rows for the /skills pane. Refreshed from
    /// the wire when the user opens /skills. Empty until the first
    /// reply.
    pub skill_entries: Vec<SkillEntry>,
    /// The /skills pane drill-down level: 0 = list, 1 = selected skill
    /// detail (body + usage + disable toggle). Mirrors the hooks pane pattern.
    pub skill_level: Cell<u8>,
    /// The selected skill index in the /skills Level-0 list.
    pub skill_sel: Cell<usize>,
    /// Session-scoped disabled skills (toggled via t in the detail view).
    /// Persisted disable is a follow-up (settings wire).
    pub skill_disabled: HashSet<String>,
    /// Whether the @ skill-picker overlay is open (typing @ in the input).
    pub skill_picker_open: bool,
    /// Selected index in the @ skill-picker.
    pub skill_picker_sel: Cell<usize>,
    /// The /hooks pane drill-down level: 0 = event list, 1 = selected event
    /// detail (registered hooks + description). A
    /// select-event → view-hook browse pattern.
    pub hooks_level: Cell<u8>,
    /// The selected event index in the /hooks Level-0 list.
    pub hooks_sel: Cell<usize>,
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
    /// True when a /status command is awaiting a wire reply. The periodic
    /// poll updates the cache silently; a command-initiated poll also renders
    /// the full status block as a transcript line on reply. Cleared on reply.
    pub pending_status_command: bool,
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
