//! The frontend's presentation and interaction state.
//!
//! Ui owns where the user is looking, what the user has typed but not sent,
//! what the pointer has selected, and the transient notices and clipboard
//! port the interaction loop reads. It holds no connection handles and no
//! authoritative session record; the session stays the source of truth for
//! everything the server returned, and views read it through App. A pane
//! that shows fetched content on demand keeps its display copy here, so a
//! drilled-in teammate's lines, an opened skill body, and an open search
//! view's corpus are Ui state whose loss costs a re-fetch or a rebuild,
//! never a dropped record.

pub(crate) mod counts;
pub mod enums;
pub(crate) mod layout_feedback;
pub(crate) mod layout_snapshot;
pub mod notifications;
pub mod render_cache;
pub(crate) mod renderer;
pub(crate) mod rows;
pub mod selection;
pub(crate) mod selections;
pub(crate) mod toggle_hint;

mod navigation;

use std::cell::Cell;
use std::io;
use std::mem;
use std::sync::Arc;

use crate::fleet::strip_state::FleetStripState;
use crate::hooks_state::HooksPaneState;
use crate::input::Input;
use crate::list_pane_state::ListPaneState;
use crate::memory_state::MemoryPaneState;
use crate::notifications::NotificationState;
use crate::permission_pane_state::PermissionPaneState;
use crate::records::TeammateView;
use crate::renderer::{PreparedFrame, Renderer};
use crate::selection::ClipboardWriter;
use crate::selections::Selections;
use crate::skills_state::SkillsPaneState;
use crate::state::model_draft::ModelDraft;
use crate::state::transcript::view::TranscriptViewState;
use crate::state::{ParkedViewStates, Stage, TrajectoryPaneState, ViewportMode};

pub use crate::ui::navigation::Navigation;

/// Presentation and interaction state for the visible frontend: navigation,
/// input, the transcript and pane view states, the verbose render flag, the
/// spinner's displayed token count, the last-written terminal title, the
/// display copies panes fetch on demand, selection, notifications,
/// clipboard, focus, the quit flag, and the draw pass's caches and
/// last-frame output.
pub struct Ui {
    /// Where the user is looking: the screen, the pane, the two viewport
    /// modes, and the overlay pickers.
    pub navigation: Navigation,
    /// The input subsystem: the chat editor, the prompt-history navigation,
    /// and the two paste halves, held as one value so a session switch resets
    /// them together.
    pub(crate) input: Input,
    /// The transcript view's own state, as one value the view on screen
    /// holds: the two scroll objects, the window flag, the search corpus and
    /// query state, the byte window, the four expansion sets, and the
    /// unseen-event cursor.
    pub(crate) transcript_view: TranscriptViewState,
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
    /// Verbose render: force results, reasoning, and fold groups expanded
    /// with untruncated chips. Set in the search view, cleared on exit.
    pub verbose: bool,
    /// The permission pane state: the selected tab, the cursor row, the
    /// typed-input sub-mode, and the pane-local search buffer. Pub(crate) so
    /// the key handler and render path reach a field directly; the whole value
    /// is what a tab switch or input-mode entry replaces.
    pub(crate) permission_pane: PermissionPaneState,
    /// The /skills pane view state: the list cursor and the open detail
    /// (body request, body text, scroll position). Kept together so the
    /// pane's view state is one owner, not a scatter of flat fields.
    pub(crate) skills_pane: SkillsPaneState,
    /// The hooks pane state: the drill-down level (0 = event list, 1 = the
    /// selected event detail) and the selected event index. Pub(crate) so the
    /// key handler and render path step a field directly; the level and the
    /// selection move together on enter and return.
    pub(crate) hooks_pane: HooksPaneState,
    /// The /memory pane's interaction state: the scope tab, the list cursor
    /// and search query, and the detail drill-down. The entry list and the
    /// toggle states these index are the session's memory capability; the
    /// compositions across the two owners are App methods. Resets with its
    /// pane peers on a session switch.
    pub(crate) memory: MemoryPaneState,
    /// The /agents pane's list cursor: which returned-delegation row the
    /// marker highlights. The rows are the session's record; the cursor is
    /// pane interaction state, kept within the rows by every rebuild, step,
    /// and read, and reset on a session switch.
    pub agents_sel: usize,
    /// The footer fleet strip's interaction state: the highlighted child row
    /// and the elapsed counters' repaint pacing. The child snapshots this
    /// index and the geometry a draw produced are the session's fleet
    /// capability and that frame's layout snapshot respectively; the
    /// compositions across the owners are App methods. Resets with its pane
    /// peers on a session
    /// switch.
    pub(crate) fleet: FleetStripState,
    /// The /model pane's draft: the row the focus sits on, the level and
    /// tier the user adjusted, the setting the arrows take, and the dirty
    /// flag a landing snapshot respects. The catalog and applied facts the
    /// draft resolves against are the session's model capability; the
    /// compositions across the two owners are App methods. Resets with its
    /// pane peers on a session switch.
    pub(crate) model: ModelDraft,
    /// The /trajectory pane's drill level, cursor, and selection. One owner
    /// for the pane's position, because the selection and the cursor are kept
    /// consistent by the same transitions.
    pub trajectory: TrajectoryPaneState,
    /// Cursor + search query for the /worktrees pane.
    pub worktree_list: ListPaneState,
    /// /worktrees pane drill-down: 0 = list, 1 = detail.
    pub worktree_level: Cell<u8>,
    /// Last token count displayed by the spinner. Lerps toward the actual
    /// count each frame for a smooth increment animation.
    pub displayed_tokens: Cell<u32>,
    /// The terminal-window title last written. The status sync writes a new
    /// title only when it differs from this, and a session switch resets it
    /// to None so the next session's title always lands.
    pub last_title: Option<String>,
    /// The four per-region drag selections: the transcript's main selection
    /// and the status, pane, and approval selections, each in its own
    /// coordinate space so a drag on one region never collides with another.
    pub(crate) selection: Selections,
    /// Transient notification toast: one-line auto-expiring hint above the
    /// input box (copy feedback, exit-again prompt). Poll-driven expiry.
    pub notifications: NotificationState,
    /// Pluggable clipboard writer. Production holds a SystemClipboard
    /// (pbcopy/OSC 52); adversarial selection tests inject a
    /// RecordingClipboard so the exact copied text can be asserted without
    /// touching the OS clipboard. Arc<dyn> so the frontend state stays
    /// Send + Sync across the TUI/runner.
    pub clipboard: Arc<dyn ClipboardWriter>,
    /// Whether the terminal window has focus (FocusGained/FocusLost events).
    /// The input cursor (invert) gates on this so the caret hides when the
    /// window is unfocused. Defaults true (assume focused at startup).
    pub terminal_focused: bool,
    /// Whether the user asked to leave the TUI loop (the Exit command, Esc
    /// or q on a login screen, a declined trust verdict, or a no-builder
    /// resume). Read back by the event loop as its stop condition; a session
    /// swap that lands in-process resets it to false.
    pub quit: bool,
    /// The draw pass's reusable caches: the per-line parse cache and the
    /// transcript's pre-rendered slot rows. A line or a slot row built at one
    /// width or expansion is reused across frames and transcript rebuilds; a
    /// session switch clears both so stale entries never ride into the new
    /// session.
    pub(crate) renderer: Renderer,
    /// The last successfully drawn frame: region rectangles, measured
    /// feedback, and the per-row keys. The draw writes into a staged frame
    /// and this one is replaced only when the draw reports success, so a
    /// reader never acts on a half-written frame.
    pub(crate) frame: PreparedFrame,
}

impl Ui {
    /// Sync the viewport and overlays to a stage: implement and verify fold
    /// into Focus (closing the palette and transcript search first), the rest
    /// unfold to Working. Owned here because the fold closes both the
    /// navigation palette and the transcript search, preserving the close
    /// before the viewport assigns.
    pub(crate) fn sync_viewport_to_stage(&mut self, stage: Stage) {
        let next = ViewportMode::for_stage(stage);
        if next != ViewportMode::Working {
            self.navigation.palette.close();
            self.transcript_view.exit_search_if_active();
        }
        self.navigation.viewport = next;
    }

    /// Reset the session-local fields a switch must clear so the new session
    /// renders as if fresh rather than layered over the old one's pane state,
    /// caches, or selection. The navigation overlays, the transcript view, the
    /// pane states (permission, skills, hooks, memory, model, fleet), the
    /// trajectory and worktree cursors, the agents cursor, the selection, the
    /// notifications, the displayed token count, the last title, the render
    /// caches, and the last frame's measured layout all reset. The
    /// process-level resources (terminal focus, clipboard) and the
    /// render-measured frame feedback stay; the input and parked view states
    /// are carried by the caller at the field level because they mix
    /// session-scoped and carry-across fields.
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.navigation.reset_for_session_switch();
        self.transcript_view = Default::default();
        self.verbose = false;
        self.worktree_list = Default::default();
        self.worktree_level = Cell::new(0);
        self.trajectory = Default::default();
        self.agents_sel = 0;
        self.notifications = Default::default();
        self.displayed_tokens = Cell::new(0);
        self.selection = Default::default();
        self.teammate_view = None;
        self.fleet = Default::default();
        self.permission_pane = Default::default();
        self.skills_pane = Default::default();
        self.hooks_pane = Default::default();
        self.memory = Default::default();
        self.model = Default::default();
        self.last_title = None;
        self.renderer.reset_for_session_switch();
        self.frame.reset_for_session_switch();
    }

    /// Take the last published frame aside so the draw writes into a fresh
    /// one. The caller hands the returned value back to settle_frame once the
    /// draw reports its outcome. The caches the draw reuses live on the
    /// renderer, so staging a frame does not drop them.
    pub(crate) fn stage_frame(&mut self) -> PreparedFrame {
        mem::take(&mut self.frame)
    }

    /// Publish the frame the draw just wrote, or put the last published one
    /// back when the draw failed. Either way the Ui holds a frame a reader
    /// may act on: a failed draw never leaves a half-written one in place.
    pub(crate) fn settle_frame<T>(
        &mut self,
        published: PreparedFrame,
        drawn: io::Result<T>,
    ) -> io::Result<T> {
        if drawn.is_err() {
            self.frame = published;
        }
        drawn
    }
}

#[cfg(test)]
mod tests;
