//! The display-row set the draw pass pre-renders and caches by version: the
//! rows themselves, the four per-row key vectors that travel with them, the
//! pre-rendered lines, and the version stamp that guards the whole set. One
//! owner because the six are recomputed in lockstep when the version is stale
//! and read in lockstep every frame: a flat field per vector invited a write
//! that bumped the version and re-filled the rows but left a key vector at the
//! old length, so a visible-window slice into a stale vector returned a key
//! for the row beside the one it was built for.

use std::cell::{Cell, RefCell};

use crate::records::ToolOutcome;
use crate::row_id::RowId;
use ratatui::text::Line;

/// The cached display-row set: the rows, the four per-row key vectors, the
/// pre-rendered lines, and the version stamp that invalidates them together.
/// Every field is a RefCell or Cell so the draw pass can borrow one vector
/// while another is still out, matching the per-vector visible_window reads.
#[derive(Debug, Default)]
pub(crate) struct DisplayRowsCache {
    /// The full pre-visible row computation (display_slots + row formatting),
    /// invalidated by the version counter. Recomputed only when the
    /// transcript or display inputs change, not every frame.
    pub(crate) rows: RefCell<Vec<(u8, String, Option<ToolOutcome>)>>,
    /// The version the cached rows and key vectors were built at. u64::MAX at
    /// start so the first frame always rebuilds; the draw pass compares it to
    /// the version it would build and rebuilds on mismatch.
    pub(crate) version: Cell<u64>,
    /// The call id each cached row carries, parallel to rows. Sliced by the
    /// same visible_window the rows are, so a screen row maps to its call id.
    pub(crate) callids: RefCell<Vec<Option<String>>>,
    /// The fold-group key each cached row belongs to, parallel to rows.
    pub(crate) fold_keys: RefCell<Vec<Option<String>>>,
    /// The expanded-group mark each cached row carries, parallel to rows.
    pub(crate) expanded_group: RefCell<Vec<Option<String>>>,
    /// The ThoughtFor turn id each cached row carries, parallel to rows.
    pub(crate) turn_ids: RefCell<Vec<Option<RowId>>>,
    /// The pre-rendered line each cached row carries (the formatted text a
    /// live re-render would otherwise rebuild), parallel to rows.
    pub(crate) pre_rendered: RefCell<Vec<Option<Line<'static>>>>,
}
