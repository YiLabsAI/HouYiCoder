//! State and transitions for the trajectory pane's drill-down.
//!
//! The pane draws every frame from a shared reference, so the values the draw
//! path writes while it renders are cells: the cursor it clamps, the body
//! length it stashes for the key handler, the background flag it reads off the
//! drilled row, and the selection it drops when the history changed. The rest
//! of the pane's position lives here rather than as parallel fields on the
//! application shell.
//!
//! The loaded window slides: an older page is prepended and the page furthest
//! from the walk is dropped. A row index cannot carry the selection across
//! that, because the index would then name a different turn, so the selection
//! is the session turn number and the row is found from it again. Mapping a row
//! to that number is the view's business, so this type holds facts only.

use std::cell::{Cell, RefCell};

/// A turn's identity in the durable log: the event that opened it.
///
/// Opaque on purpose. The pane compares keys to know which turn a row is, and
/// the composition root resolves one back to the bytes it came from; neither
/// needs the other's view of it. A turn number cannot do this, because numbers
/// begin again when a session is cleared.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct TrajectoryTurnKey(String);

impl TrajectoryTurnKey {
    /// The key of the event that opened a turn, as its durable id reads.
    pub fn from_opening_event(id: &str) -> Self {
        Self(id.to_string())
    }

    /// The durable id this key was built from.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The turn the pane's drill is about: its durable key, and the history the key
/// was read in.
///
/// The drill's identity is the key, not the number: a page arriving under the
/// row moves the number's meaning, and a clear makes the number name another
/// turn. The number is what the list shows; this is what the detail is asked
/// for by.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrajectoryDrill {
    /// The turn the drill is about.
    pub key: TrajectoryTurnKey,
    /// The history the key was read in.
    pub history_generation: u64,
}

/// The turn the pane is on: the number it carries in the history in hand, and
/// which history that is.
///
/// One value rather than two fields, so a caller cannot move the number
/// without saying which history it belongs to. A clear starts a history whose
/// turn numbers begin again, and a number from the old one names a different
/// turn there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrajectorySelection {
    /// The session turn number the selection names.
    pub number: usize,
    /// The history the number was read in.
    pub history_generation: u64,
}

/// Where the trajectory pane is: which drill level, which row the cursor is
/// on, and which session turn the selection names.
#[derive(Default)]
pub struct TrajectoryPaneState {
    /// 0 = turn list, 1 = turn detail (events + ASCII bar), 2 = event detail
    /// (full data).
    level: Cell<u8>,
    /// Cursor into the current level's list (turn list at level 0, event list
    /// at level 1). Clamped to the list length at render time.
    cursor: Cell<usize>,
    /// List length at the current drill level, stashed by the render path so
    /// the Up/Down handler can clamp the cursor in [0, len-1]: without it the
    /// cursor grows past the last row on Down and the selection glyph
    /// vanishes, because no row matches the out-of-range index.
    list_len: Cell<usize>,
    /// The L0-selected row index, frozen on drill so L1/L2 render the row the
    /// user picked rather than the first turn.
    turn_idx: Cell<usize>,
    /// True when the L0 row is a background event, which skips the L2 drill.
    at_bg: Cell<bool>,
    /// The turn the pane is on, or None while it simply follows the tail. None
    /// is not turn zero: a turn number of zero is not a turn, and a sentinel
    /// would have to be checked at every read.
    selection: Cell<Option<TrajectorySelection>>,
    /// The turn the drill levels are about, or None while the pane is on the
    /// list or on a background row.
    drill: RefCell<Option<TrajectoryDrill>>,
}

impl TrajectoryPaneState {
    /// The current drill level: 0 turn list, 1 turn detail, 2 event detail.
    pub fn level(&self) -> u8 {
        self.level.get()
    }

    /// Move to a drill level.
    pub fn set_level(&self, level: u8) {
        self.level.set(level);
    }

    /// The cursor's row within the current level's list.
    pub fn cursor(&self) -> usize {
        self.cursor.get()
    }

    /// Move the cursor to a row of the current level's list.
    pub fn set_cursor(&self, cursor: usize) {
        self.cursor.set(cursor);
    }

    /// The length of the current level's list, as the render path measured it.
    pub fn list_len(&self) -> usize {
        self.list_len.get()
    }

    /// Record the length the Up/Down handler clamps the cursor against.
    pub fn set_list_len(&self, len: usize) {
        self.list_len.set(len);
    }

    /// The row index frozen at drill time, which L1 and L2 render.
    pub fn turn_idx(&self) -> usize {
        self.turn_idx.get()
    }

    /// Freeze the drilled row.
    pub fn set_turn_idx(&self, index: usize) {
        self.turn_idx.set(index);
    }

    /// Whether the drilled row is a background event, which has no event list.
    pub fn at_bg(&self) -> bool {
        self.at_bg.get()
    }

    /// Record whether the drilled row is a background event.
    pub fn set_at_bg(&self, at_bg: bool) {
        self.at_bg.set(at_bg);
    }

    /// The turn the pane is on, if it is on one.
    pub fn selection(&self) -> Option<TrajectorySelection> {
        self.selection.get()
    }

    /// Put the pane on a turn of the history in hand.
    pub fn select(&self, number: usize, history_generation: u64) {
        self.selection.set(Some(TrajectorySelection {
            number,
            history_generation,
        }));
    }

    /// Drop the selection, so nothing is restored from it.
    pub fn clear_selection(&self) {
        self.selection.set(None);
    }

    /// The turn the drill levels are about, if the pane is on one.
    pub fn drill(&self) -> Option<TrajectoryDrill> {
        self.drill.borrow().clone()
    }

    /// Put the drill levels on a turn of the history in hand.
    pub fn set_drill(&self, drill: TrajectoryDrill) {
        *self.drill.borrow_mut() = Some(drill);
    }

    /// Drop the drill: the pane is back on the list, or on a background row.
    pub fn clear_drill(&self) {
        *self.drill.borrow_mut() = None;
    }
}
