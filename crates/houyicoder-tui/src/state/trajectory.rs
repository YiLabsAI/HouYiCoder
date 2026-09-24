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

use std::cell::Cell;

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
    /// The session turn number the L0 cursor is on, or 0 for none.
    ///
    /// Set once the user navigates; left at 0 while the pane simply follows
    /// the tail, so the tail keeps pulling new turns into view.
    selected_turn: Cell<usize>,
    /// The history the selection was made in. A clear starts a new one whose
    /// turn numbers begin again, so a selection from another history is
    /// dropped rather than restored onto a turn it does not name.
    selected_generation: Cell<u64>,
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

    /// The session turn the selection names, or 0 when nothing is selected.
    pub fn selected_turn(&self) -> usize {
        self.selected_turn.get()
    }

    /// The history the selection belongs to.
    pub fn selected_generation(&self) -> u64 {
        self.selected_generation.get()
    }

    /// Select a turn of the history in hand.
    pub fn select_turn(&self, turn: usize, generation: u64) {
        self.selected_turn.set(turn);
        self.selected_generation.set(generation);
    }

    /// Drop the selection, so nothing is restored from it.
    pub fn clear_selection(&self) {
        self.selected_turn.set(0);
        self.selected_generation.set(0);
    }
}
