//! State for the hooks pane browse: the drill level and the selection cursor
//! in the level-0 event list. One owner so the level and the selection move
//! together — entering a hook sets level to 1, returning to the list sets it
//! back to 0 and clamps the cursor against the live event count.

use std::cell::Cell;

/// The hooks pane state. Fields are Cell-backed so the key handler and render
/// path read and step them without a mutable borrow of App, matching the
/// skills pane owner.
#[derive(Debug, Default)]
pub(crate) struct HooksPaneState {
    /// The drill level in the hooks pane: 0 is the event list, 1 is the
    /// detail of the event the cursor sat on. Enter steps in, Esc steps back.
    pub(crate) level: Cell<u8>,
    /// The selected event index in the level-0 list; clamped at render against
    /// the live event count so it never points past the last event.
    pub(crate) sel: Cell<usize>,
}
