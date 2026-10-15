//! The renderer's layout snapshot: the geometry one draw produced. Each
//! region's rectangle and the rows it rendered are written and read
//! together, so the input and click paths resolve against what was drawn
//! rather than against a recomputation. The footer budget's grant to the
//! fleet strip is here too: it is that region's measured height, and the
//! status-bar hint reads it in the same frame the draw wrote it.

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
    /// Where the footer fleet strip was drawn this frame, for the mouse
    /// router. Zero when not drawn, so a stale rect cannot swallow clicks.
    pub(crate) fleet_rect: Cell<Rect>,
    /// Where the queued-input strip was drawn this frame, for the mouse
    /// router. Zero when not drawn.
    pub(crate) queue_rect: Cell<Rect>,
    /// Rows the footer budget granted the fleet strip this frame. The
    /// status-bar hint reads it to advertise the affordance the strip can
    /// actually honor: per-row selection while rows are visible, the pane
    /// when the strip is a summary or gone.
    pub(crate) fleet_granted: Cell<u16>,
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
            fleet_rect: Cell::new(Rect::new(0, 0, 0, 0)),
            queue_rect: Cell::new(Rect::new(0, 0, 0, 0)),
            fleet_granted: Cell::new(0),
        }
    }
}
