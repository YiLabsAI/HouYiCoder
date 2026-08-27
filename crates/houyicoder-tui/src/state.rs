//! TUI state and core enums. The App struct carries both the legacy stub
//! surface (transcript, panes, stage) and the real agent-loop wiring (runner,
//! session, tokio runtime, channel). When a runner is present, submit_input
//! spawns runner.run on the runtime and the transcript is rebuilt from real
//! TurnEvents; when no runner is wired (tests, login-only), the legacy stub
//! path stays so existing tests keep passing. The view module reads App and
//! renders it; the app/keys modules mutate it in response to keys.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};

pub(crate) mod app_methods;
pub(crate) mod counts;
pub(crate) mod enums;
mod scroll;
mod search_view;
mod teammate_view;

use crate::console_state::ConsoleState;
use crate::input::InputField;
use crate::palette::PaletteState;
use crate::paste::PasteStore;
use crate::review_queue::ReviewQueue;
use crate::scroll::{SearchState, TranscriptScroll};
use crate::selection::Selection;
use houyicoder_protocol::frontend::LoginMode;
use houyicoder_protocol::frontend::SessionId;
use houyicoder_protocol::frontend::run::ApprovalRequest;
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
    AgentStatus, AuditEntry, ConsoleTodo, DiffData, Divergence, GraphResult, Hunk, HunkEvidence,
    MemoryEntry, PlanArtifact, ReviewFinding, SpecArtifact, SpecClause, Verdict, VerifyResult,
    audit_entry,
};
pub use crate::records::{Approval, SpecContext, StatusStub, TranscriptLine};
pub use crate::run_control::AgentMessage;

pub use crate::state::enums::*;

/// The full TUI state. Owned by the app loop; read by the view module. The
/// palette, console, and review-queue concerns are delegated to focused
/// sub-structs (PaletteState, ConsoleState, ReviewQueue); the remaining
/// fields are core surface + artifact state. The agent_* fields wire the real
/// agent loop: when runner is Some, submit_input spawns runner.run on the tokio runtime and the transcript is rebuilt from real TurnEvents arriving over the channel.
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
    pub transcript: Vec<TranscriptLine>,
    /// The durable wire frame history, owned by App (not the driver). The
    /// driver ships one Frame per server frame; App pushes here and the
    /// transcript projection reads from it. The source of truth for the
    /// session history.
    pub frames: Vec<crate::transcript::TranscriptFrame>,
    /// Seal cursor (frames side): frames[..sealed_frames_end] are already
    /// projected into transcript[..sealed_transcript_len] and are immutable
    /// for the current turn. The mid-run rebuild re-projects only
    /// frames[sealed_frames_end..] so per-frame cost is O(current turn), not
    /// O(whole history). Reset by rewind (frames truncate) + turn boundary.
    pub sealed_frames_end: usize,
    /// Seal cursor (transcript side): the prefix length that tracks
    /// frames[..sealed_frames_end] (frame-derived + TUI-only lines). The
    /// mid-run rebuild merges only transcript[sealed_transcript_len..].
    pub sealed_transcript_len: usize,
    /// Verdict cursor: acpx permission_decision frames are deserialized once
    /// and appended to verdict_log_cache as they cross this cursor. Avoids
    /// re-deserializing the whole history per rebuild (per-frame now). Reset
    /// to 0 (and the cache cleared) when frames truncate below it
    /// (rewind/clear).
    pub verdict_cursor: usize,
    pub transcript_scroll: TranscriptScroll,
    /// Cached display rows: the full pre-visible computation (display_slots +
    /// row formatting). Invalidated by a version counter — only recomputed
    /// when the transcript or display inputs change, not every frame.
    pub display_rows_cache:
        std::cell::RefCell<Vec<(u8, String, Option<crate::records::ToolOutcome>)>>,
    pub display_rows_version: std::cell::Cell<u64>,
    pub transcript_version: std::cell::Cell<u64>,
    pub cached_callids: std::cell::RefCell<Vec<Option<String>>>,
    pub cached_fold_keys: std::cell::RefCell<Vec<Option<String>>>,
    pub cached_expanded_group: std::cell::RefCell<Vec<Option<String>>>,
    pub cached_turn_ids: std::cell::RefCell<Vec<Option<String>>>,
    pub cached_pre_rendered: std::cell::RefCell<Vec<Option<ratatui::text::Line<'static>>>>,
    /// Frame index captured on first scroll-away (None while following). Pill
    /// counts agent segments in frames since; eviction-safe. Reset on tail return.
    pub scrolled_from_frame: Option<usize>,
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
    pub window_scroll: crate::scroll::WindowScroll,
    /// Corrupt lines skipped in the current window (separate from the
    /// whole-log search_skipped so window-mode chrome shows the per-window
    /// count).
    pub window_skipped: usize,
    /// True while the G full-scan builds the event-byte-offset index across
    /// frames (one chunk per frame keeps the UI responsive; Esc interrupts).
    /// The flat render path drives index_chunk while this is set. Cell so the
    /// draw borrow (&App) can flip it off when the build completes.
    pub indexing: std::cell::Cell<bool>,
    /// Bytes of the log indexed so far (for the indexing-percent chrome),
    /// published by the render path each frame while indexing.
    pub indexed_bytes: std::cell::Cell<u64>,
    /// Total log bytes the index covers (the frozen file size).
    pub index_total: std::cell::Cell<u64>,
    /// True when the full index is built (event_count/byte_at answer).
    pub index_done: std::cell::Cell<bool>,
    /// Optional full-history disk-search seam. None in stub / unwired modes
    /// (the /search --all flag then reports no disk results). When wired, the
    /// composition root injects an impl that reads the durable session log +
    /// projects TurnEvents to searchable text — the TUI never touches the log.
    /// Optional trajectory-data seam. The composition root injects an impl
    /// that reads the durable session log and projects events into a
    /// TrajectoryView; None in stub and unwired modes falls back to the mock
    /// trajectory so the pane still renders a demo.
    pub trajectory_log: Option<std::sync::Arc<dyn crate::view::trajectory_pane::TrajectoryLog>>,
    /// Optional export seam. The composition root injects an impl that reads
    /// the durable session log and serializes the full trajectory, tool
    /// stats, usage, checkpoints, and errors to a JSON document. None in
    /// stub or unwired modes, where /export reports "no session log wired"
    /// instead of writing an empty file.
    pub export_log: Option<std::sync::Arc<dyn crate::view::export_log::ExportLog>>,
    /// Optional transcript-snapshot seam. The composition root injects an
    /// impl that loads the durable session log into a TranscriptLine
    /// snapshot for the search view (the read-whole path for logs under
    /// the threshold). None in stub or unwired modes, where the search
    /// view falls back to the in-memory transcript vec.
    pub snapshot: Option<std::sync::Arc<dyn crate::transcript::snapshot::TranscriptSnapshot>>,
    /// The session-listing bridge for the /resume picker (lists resumable
    /// sessions with derived titles). None in stub/test bundles.
    pub session_lister: Option<std::sync::Arc<dyn crate::resume_picker::SessionLister>>,
    /// The session picker overlay state (opened by /resume with no arg).
    pub resume_picker: crate::resume_picker::SessionPickerState,
    /// A pending resume request set when the user picks a session in the
    /// picker (or /resume <id|name|file>). Carries a session id OR an export
    /// file path (the resume builder dispatches on which). The event loop's
    /// try_swap_session consumes it: with a resume_builder wired (the normal
    /// path), it builds the new bundle and swap_session swaps in place — no
    /// quit, no restart. Only when no builder is wired does it put the target
    /// back + set quit, letting the caller fall back to a fresh re-enter.
    pub pending_resume_target: Option<String>,
    pub palette: PaletteState,
    pub approval: Option<Approval>,
    /// Parallel to approval: when the model calls AskUserQuestion, the
    /// interruption is parsed into this card instead of the generic approval
    /// popup. None for plain tool-approval interruptions.
    pub ask_question: Option<crate::records::AskQuestion>,
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
    pub memory_entries: Vec<MemoryEntry>,
    /// The auto-memory / auto-dream toggle snapshot rendered as on/off rows in
    /// the /memory pane. Defaults to both on; refreshed from the wire on
    /// pane-open and after each /memory toggle flip.
    pub memory_toggles: houyicoder_protocol::frontend::memory::ToggleState,
    /// Storage-scope filter the /memory pane is narrowed to. Shift+Tab cycles
    /// All → User → Project → Auto. All shows the merged set; the others
    /// narrow to one physical root.
    pub memory_scope_tab: crate::state::enums::MemoryScopeTab,
    /// Cursor + search query for the /memory pane. The cursor indexes the
    /// scope-and-text-filtered list; move_cursor/clamp take the filtered
    /// length. The query composes with the scope tab (both must match).
    /// Adopted from ListPaneState (the worktree pane was the first adopter).
    pub memory_list: crate::list_pane_state::ListPaneState,
    /// The linked-worktree rows for the /worktrees pane. Refreshed from
    /// parse_worktrees on pane-open. Empty until the user opens the pane (no
    /// background poll — the list is cheap and the pane is one-shot).
    pub worktree_entries: Vec<crate::composition::WorktreeEntry>,
    /// Cursor + search query for the /worktrees pane. The first pane to
    /// adopt ListPaneState; others migrate on touch-ratchet.
    pub worktree_list: crate::list_pane_state::ListPaneState,
    /// /worktrees pane drill-down: 0 = list, 1 = detail.
    pub worktree_level: std::cell::Cell<u8>,
    /// /trajectory pane drill-down state: 0 = turn list, 1 = turn detail
    /// (events + ASCII bar), 2 = event detail (full data).
    pub trajectory_level: std::cell::Cell<u8>,
    /// Cursor into the current level's list (turn list at level 0, event
    /// list at level 1). Clamped to the list length at render time.
    pub trajectory_cursor: std::cell::Cell<usize>,
    /// List length at the current drill level, stashed by the render path so
    /// the Up/Down key handler can clamp the cursor in [0, len-1] — without
    /// this the cursor grows past the last row on Down and the selection
    /// glyph vanishes (no row matches the out-of-range index).
    pub trajectory_list_len: std::cell::Cell<usize>,
    /// The L0-selected row index, frozen on drill so L1/L2 render the row
    /// the user picked (not always the first turn — drilling a later turn or
    /// a [bg] row showed the first turn's events before this field existed).
    pub trajectory_turn_idx: std::cell::Cell<usize>,
    /// True when the L0 row is a bg event (skips L2 drill-in).
    pub trajectory_at_bg: std::cell::Cell<bool>,
    pub agents: Vec<AgentStatus>,
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
    /// The tokio runtime that drives async run/resume. None in stub mode.
    pub runtime: Option<Arc<tokio::runtime::Runtime>>,
    /// Sender cloned into each spawned task; the task ships the RunResult plus
    /// the session replay back over this channel.
    pub agent_tx: Option<mpsc::Sender<AgentMessage>>,
    /// The live session with the engine: owns the command channel to the
    /// driver, the message channel back to the event loop, the request-id
    /// counter, and the driver task handle. None in the pure-stub path.
    pub session: Option<crate::session::Session>,
    /// The reverse-request req_id of the currently-shown permission ask,
    /// echoed back with the verdict. None when no approval card is up.
    pub pending_permission_req_id:
        std::cell::Cell<Option<houyicoder_protocol::envelope::RequestId>>,
    /// True while a run or resume is in flight, so a second Enter queues
    /// instead of stacking a second run.
    pub agent_busy: bool,
    /// Whether the terminal window has focus (FocusGained/FocusLost events).
    /// The input cursor (invert) gates on this so the caret hides when the
    /// window is unfocused, following a renderPlaceholder terminal
    /// focus gate. Defaults true (assume focused at startup).
    pub terminal_focused: bool,
    pub active_run_req_id: std::cell::Cell<Option<houyicoder_protocol::envelope::RequestId>>,
    /// When the current run started (set on spawn, cleared on completion)
    /// so the spinner row can show elapsed time and animate its glyph.
    pub run_started: Option<std::time::Instant>,
    /// When the session's first run started, for end-to-end elapsed.
    pub session_started_at: Option<std::time::Instant>,
    /// Cumulative output tokens across all turns this session.
    pub cumulative_tokens: u64,
    /// Cumulative model-call steps across all turns.
    pub cumulative_steps: u32,
    /// The session checklist from the wire stream. Last-write-wins; rebuilt
    /// from the full frame list each batch.
    pub todos_cache: Vec<crate::todo_view::TodoView>,
    /// Whether the collapsed checklist is force-expanded inline.
    pub todo_expanded: bool,
    /// When each checklist item transitioned to Completed, keyed by content.
    /// Drives the 30-second recent-completed visibility window in the
    /// collapsed checklist. Updated in accumulate_wire_state.
    pub todo_completion_at: HashMap<String, std::time::Instant>,
    /// Last terminal height seen by the draw pass, stashed for height-aware
    /// checklist rendering. Interior-mutable for draw-borrow updates.
    pub last_terminal_rows: Cell<u16>,
    /// Last transcript PANE width (inner area.width, not the input last_cols).
    /// Stashed so the count path soft-wraps to the same width render used
    /// (count == render). 0 before first render = do-not-wrap.
    pub last_transcript_width: Cell<u16>,
    /// Transient assistant text accumulated from streamed deltas. Appended per
    /// Delta message; cleared and replaced by the durable projection on Done.
    /// Live preview only — the session log is the source of truth.
    pub live_assistant_text: String,
    /// True while a streamed turn is in flight and live_assistant_text holds a
    /// preview the Done message will replace. Drives the live-row render.
    pub live_active: bool,
    /// Transient reasoning preview from streamed ReasoningDelta chunks.
    /// Cleared on Done. Held for the post-turn ThoughtFor summary, not
    /// echoed live (the live indicator is the spinner verb, see live_block).
    pub live_reasoning_text: String,
    /// The content block currently streaming during a live turn. A
    /// ReasoningDelta flips it to Thinking, an assistant-text Delta to
    /// Responding. The spinner verb shows Thinking only while this is
    /// Thinking (plus the 2-second min-display hold), else Working — so the
    /// verb tracks the active block, not whether any reasoning streamed.
    pub live_block: crate::state::enums::LiveBlock,
    /// When the reasoning phase started. Enforces a 2-second minimum
    /// display of the Thinking verb. Cleared on Done.
    pub thinking_started_at: Option<std::time::Instant>,
    /// Last token count displayed by the spinner. Lerps toward the actual
    /// count each frame for a smooth increment animation.
    pub displayed_tokens: std::cell::Cell<u32>,
    /// When the last streamed Delta arrived. None until the first delta;
    /// drives the spinner stall gradient after STALL_THRESHOLD_SECS.
    /// Cleared on spawn_run/spawn_resume for a grace period.
    pub last_delta_at: Option<std::time::Instant>,
    /// Call ids of tool calls currently executing (ToolCall seen, no terminal
    /// update yet). Drives the spinner's tool-use breathing pulse and exempts
    /// tool runtime from the stall gradient. Cleared on Done.
    pub running_tools: HashSet<String>,
    /// Per-call progress for long-running tools (bash): elapsed seconds +
    /// the running stdout line count (None when the backend does not stream
    /// stdout). The runner ticks ToolProgress every ~1s; the chip renders
    /// (Ns) after 2s, or (Ns · M lines) when lines is Some. Cleared when
    /// the tool result lands (retire_tool) + on Done.
    pub bash_progress: HashMap<String, BashProgress>,
    /// The user input that started the in-flight run, so an abort with no
    /// real content can restore it to the input box.
    pub last_run_input: Option<String>,
    /// Pending approval requests from the last Interruption. The popup shows
    /// the first; the verdict applies to all (batch decide). Cleared on resume.
    pub pending_approvals: Vec<ApprovalRequest>,
    /// Queued user inputs submitted while a run was in flight (FIFO). A
    /// Typed queue (messages + slash commands); drained FIFO at idle.
    pub pending: Vec<crate::pending_queue::PendingItem>,
    /// Whether the queue-management overlay (Ctrl+G) is open over the
    /// transcript, listing every queued item with per-item edit/delete.
    pub queue_view_open: bool,
    /// Cursor index into pending while the queue-management overlay is
    /// open (0 = first queued item). Wraps on navigation.
    pub queue_focus: usize,
    /// In-app text selection (drag-select in the transcript, copy on release).
    pub selection: Selection,
    /// Last-rendered transcript rect (screen coords), stashed by the draw
    /// pass so the mouse handler can map a click cell to a transcript row.
    pub transcript_rect: Cell<Rect>,
    /// Last-rendered queued-input footer strip rect (screen coords), stashed
    /// by the draw pass so the mouse handler can map a click to a queued item
    /// (click to recall into the input box for editing).
    pub queue_rect: Cell<Rect>,
    /// Last-rendered "jump to bottom" pill rect; hit-tested before the
    /// transcript surface. Zero rect when hidden.
    pub jump_pill_rect: Cell<Rect>,
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
    /// Per-line render cache (content hash + width + expand key; indices are
    /// unstable — transcript rebuilds each batch). Count + render share it.
    pub render_cache: RefCell<crate::render_cache::RenderCache>,
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
    /// Per-ThoughtFor-line expansion state, keyed by reasoning text (stable
    /// across rebuilds) so Ctrl+O expands that turn's reasoning inline. Empty
    /// = collapsed.
    pub expanded_thinking: HashSet<String>,
    pub expanded_subagents: HashSet<String>,
    /// Drilled-in teammate transcript; when Some, active_transcript swaps to
    /// the child's turns with a banner. Enter opens, Esc closes.
    pub teammate_view: Option<crate::records::TeammateView>,
    /// Footer fleet state: the child snapshots + the Shift-arrow selection.
    pub fleet: crate::agent_message::FleetState,
    /// Verbose render: force results, reasoning, and fold groups expanded
    /// with untruncated chips. Set in the search view, cleared on exit.
    pub verbose: bool,
    /// Session counter minting a stable unique turn_id for each completed
    /// turn's ThoughtFor line (incremented at FinalOutput Done). Drives
    /// ThoughtFor.turn_id so expand/collapse state keys off turn identity,
    /// not reasoning text (which can collide across turns).
    pub turn_seq: u64,
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
    pub paste_last: Option<std::time::Instant>,
    /// Last computed input wrap column count (set by the draw pass, read by
    /// key handlers for cursor up/down in wrapped space). Interior-mutable so
    /// the draw borrow of App can update it without going through &mut.
    pub last_cols: std::cell::Cell<usize>,
    /// The permission mode cache, wire-typed. Seeded once on the first idle
    /// poll (so the status-bar pill renders from session start), then updated
    /// by the PermissionMode / PermissionCycleMode responses the server ships
    /// on Shift+Tab cycle. The server is the single write authority for mode;
    /// the TUI never imports the permission crate's gate.
    pub mode_cache: Option<houyicoder_protocol::frontend::permission::PermissionMode>,
    /// Durable rule cache (wire-typed), refreshed by PermissionRulesResult.
    pub rules_cache: Vec<houyicoder_protocol::frontend::permission::PermissionRule>,
    pub dirs_cache: Vec<String>,
    /// Ask-before-git checkpoint toggle cache (default on); /permission git refreshes it.
    pub ask_before_git_enabled: bool,
    /// /context cache: last breakdown, rendered immediately on /context (refreshed in background). None until the first ContextResult.
    pub context_cache: Option<houyicoder_protocol::frontend::context::ContextBreakdown>,
    /// Per-tool last-used verdict (identity, not list position). The cursor
    /// preselect reads this so rejecting bash lands on No next time.
    /// Session-scoped; not persisted across processes.
    pub sticky_choices:
        std::collections::HashMap<String, houyicoder_protocol::acp_wire::PermissionOptionKind>,
    /// The session verdict log, from the acpx/context/permission_decision stream — the client-side audit trail of every approve/deny.
    pub verdict_log_cache: Vec<houyicoder_protocol::frontend::permission::PermissionDecisionEntry>,
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
    /// The last wire status snapshot, cached from the periodic poll the event loop drives while a carrier is wired. The per-frame status bar plus /sandbox and /compact read this so they never call the engine runner. None in stub mode (render falls back to a zeroed stub).
    pub status_cache: Option<houyicoder_protocol::frontend::status::StatusSnapshot>,
    /// When the last periodic StatusQuery shipped. Fires every
    /// STATUS_POLL_INTERVAL_SECS so the bar + /sandbox stay recent.
    pub last_status_poll: Option<std::time::Instant>,
    /// The registered-hook rows for the /hooks pane. Refreshed from the wire
    /// (HooksResult) when the user opens /hooks. Empty until the first reply.
    pub hook_entries: Vec<houyicoder_protocol::frontend::hooks::HookEntry>,
    pub tool_entries: Vec<houyicoder_protocol::frontend::tools::ToolEntry>,
    /// The /hooks pane drill-down level: 0 = event list, 1 = selected event
    /// detail (registered hooks + description). A
    /// select-event → view-hook browse pattern.
    pub hooks_level: std::cell::Cell<u8>,
    /// The selected event index in the /hooks Level-0 list.
    pub hooks_sel: std::cell::Cell<usize>,
    pub projected_from_frame: std::cell::Cell<usize>,
    /// The current model tier label in the /model pane. The active row renders
    /// with a check; the provider model id updates on select.
    pub model_tier: String,
    /// The /model pane cursor (Up/Down moves, Enter selects); clamped to list len.
    pub model_sel: usize,
    /// The /model pane catalog (ModelInfo reply); empty until it lands.
    pub model_catalog: houyicoder_protocol::frontend::model::ModelCatalog,
    /// Applied effort (ModelApplied reply); None hides the badge.
    pub applied_effort: Option<houyicoder_protocol::llm::EffortLevel>,
    /// Picker effort pick (None = auto); updated by arrows.
    pub model_effort: Option<houyicoder_protocol::llm::EffortLevel>,
    /// True once arrows pressed; cursor-move stops clobbering.
    pub model_effort_toggled: bool,
    /// The active /status sub-tab (Status / Config / Usage). Tab or Left/Right
    /// cycles it; the pane header renders the three titles with the active one
    /// highlighted. A Settings-modal-style multiple tabs.
    pub status_tab: crate::state::enums::StatusTab,
    /// In-place session-name edit buffer for the status Status tab. None
    /// unless the user pressed e on the Status tab to rename; the pane
    /// renders the name row as an editable input + the keys route char,
    /// backspace, Left/Right to the buffer. Enter commits (sends a
    /// RenameSession request), Esc cancels. Houyi makes the session name
    /// inline-editable (rather than a rename command).
    pub status_name_edit: Option<crate::input::InputField>,
    pub last_title: Option<String>,
    /// True when a /status command is awaiting a wire reply. The periodic
    /// poll updates the cache silently; a command-initiated poll also renders
    /// the full status block as a transcript line on reply. Cleared on reply.
    pub pending_status_command: bool,
    /// True between an Esc-abort and the matching Done(Interrupted). Set in
    /// abort_run when the session/cancel notification ships; cleared in the
    /// Done handler. The honest form for a wire abort: the token fire is
    /// async (a round-trip to the server), so the UI shows a cancelling state
    /// until the run resolves. In Mode B (cross-process) the direct token
    /// does not physically exist, so this flag is the only abort surface.
    pub cancelling: bool,
    /// Pluggable clipboard writer. Production holds a SystemClipboard
    /// (pbcopy/OSC 52); adversarial selection tests inject a RecordingClipboard
    /// so the exact copied text can be asserted without touching the OS
    /// clipboard. Arc<dyn> so App stays Send + Sync across the TUI/runner.
    pub clipboard: Arc<dyn crate::selection::ClipboardWriter>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("screen", &self.screen)
            .field("stage", &self.stage)
            .field("pane", &self.pane)
            .field("viewport", &self.viewport)
            .field("transcript_len", &self.transcript.len())
            .field("agent_busy", &self.agent_busy)
            .field("pending_approvals", &self.pending_approvals.len())
            .field("quit", &self.quit)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_palette_nav_no_panic() {
        let mut app = crate::composition::app();
        app.open_palette();
        app.palette_up();
        app.palette_down();
        app.palette_push('a');
        app.palette_pop();
    }

    #[test]
    fn test_console_focus_nav() {
        let mut app = crate::composition::app();
        app.console_focus_up();
        app.console_focus_down();
    }

    #[test]
    fn test_app_debug_format() {
        let app = crate::composition::app();
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
