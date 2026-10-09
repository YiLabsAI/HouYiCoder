//! The renderer's layout snapshot: per-region rectangles and rendered-row
//! caches the draw pass stashes, read back by the input and click paths as
//! the last drawn frame's geometry. One owner because each region's rect
//! and rows are written and read together.

use std::cell::{Cell, RefCell};

use ratatui::layout::Rect;

#[derive(Debug)]
pub(crate) struct LayoutSnapshot {
    pub(crate) transcript_rect: Cell<Rect>,
    pub(crate) jump_to_bottom_rect: Cell<Rect>,
    pub(crate) last_transcript_rows: RefCell<Vec<(u8, String)>>,
    pub(crate) last_all_rows: RefCell<Vec<(u8, String)>>,
    pub(crate) pane_rect: Cell<Rect>,
    pub(crate) last_pane_rows: RefCell<Vec<(u8, String)>>,
    pub(crate) status_rect: Cell<Rect>,
    pub(crate) last_status_rows: RefCell<Vec<(u8, String)>>,
    pub(crate) approval_rect: Cell<Rect>,
    pub(crate) last_approval_rows: RefCell<Vec<(u8, String)>>,
}

impl Default for LayoutSnapshot {
    fn default() -> Self {
        Self {
            transcript_rect: Cell::new(Rect::new(0, 0, 0, 0)),
            jump_to_bottom_rect: Cell::new(Rect::new(0, 0, 0, 0)),
            last_transcript_rows: RefCell::new(Vec::new()),
            last_all_rows: RefCell::new(Vec::new()),
            pane_rect: Cell::new(Rect::new(0, 0, 0, 0)),
            last_pane_rows: RefCell::new(Vec::new()),
            status_rect: Cell::new(Rect::new(0, 0, 0, 0)),
            last_status_rows: RefCell::new(Vec::new()),
            approval_rect: Cell::new(Rect::new(0, 0, 0, 0)),
            last_approval_rows: RefCell::new(Vec::new()),
        }
    }
}
