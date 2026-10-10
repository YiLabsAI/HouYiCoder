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

mod navigation;

use std::cell::Cell;
use std::sync::Arc;

use crate::hooks_state::HooksPaneState;
use crate::input::Input;
use crate::list_pane_state::ListPaneState;
use crate::notifications::NotificationState;
use crate::permission_pane_state::PermissionPaneState;
use crate::records::TeammateView;
use crate::selection::ClipboardWriter;
use crate::selections::Selections;
use crate::skills_state::SkillsPaneState;
use crate::state::transcript::view::TranscriptViewState;
use crate::state::{ParkedViewStates, QueueViewState, TrajectoryPaneState};

pub use crate::ui::navigation::Navigation;

/// Presentation and interaction state for the visible frontend: navigation,
/// input, the transcript and pane view states, the verbose render flag, the
/// spinner's displayed token count, the last-written terminal title, the
/// display copies panes fetch on demand, selection, notifications,
/// clipboard, focus, and the queued-input hit-test geometry.
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
    /// The /agents pane's list cursor: which returned-delegation row the
    /// marker highlights. The rows are the session's record; the cursor is
    /// pane interaction state, kept within the rows by every rebuild, step,
    /// and read, and reset on a session switch.
    pub agents_sel: usize,
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
    /// Selection and hit-test state for the queued-input interface. The
    /// queue's own cursor lives on the InputQueue; this holds only the
    /// last-rendered strip rectangle the pointer handler reads back.
    pub queue_view: QueueViewState,
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
}
