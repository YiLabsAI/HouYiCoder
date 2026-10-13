//! The draw pass's state: the caches that outlive a frame and the last
//! frame's measured layout, feedback, and keys.

use std::cell::RefCell;

use crate::layout_feedback::LayoutFeedback;
use crate::layout_snapshot::LayoutSnapshot;
use crate::render_cache::RenderCache;
use crate::rows::display_rows_cache::DisplayRowsCache;
use crate::rows::last_row_keys::LastRowKeys;

/// The draw pass's reusable caches. Both survive a frame and are cleared
/// together on a session switch, because a line or a slot row parsed for the
/// old session must not ride into the new one at a different width or
/// expansion. Interior mutability keeps them writeable through the draw
/// pass, which receives an immutable App.
#[derive(Default)]
pub(crate) struct Renderer {
    /// Per-line parse results: a line rendered at one width or expansion is
    /// reused across frames and transcript rebuilds.
    pub(crate) lines: RefCell<RenderCache>,
    /// Per-slot rows: the transcript's pre-rendered rows, kept until the key
    /// they were built for goes stale.
    pub(crate) slots: DisplayRowsCache,
}

/// The last successfully drawn frame: the measured region rectangles, the
/// feedback dimensions the count and layout paths read back, and the per-row
/// keys the click and Ctrl+O paths resolve against. The draw writes into a
/// staged frame and this one is replaced only when the draw reports success,
/// so a reader never acts on a half-written frame. A region the frame did not
/// draw reads back empty rather than carrying the previous frame's geometry.
/// The caches this frame was built from live on the Renderer: they outlive
/// the frame, so a new frame reuses them instead of rebuilding.
#[derive(Default)]
pub(crate) struct PreparedFrame {
    pub(crate) layout: LayoutSnapshot,
    pub(crate) feedback: LayoutFeedback,
    /// Behind one borrow: the draw pass replaces the whole key set from the
    /// row set it just sliced, so a reader never combines keys from one frame
    /// with rows from another.
    pub(crate) keys: RefCell<LastRowKeys>,
}

impl Renderer {
    /// Clear both caches so nothing parsed or pre-rendered for the old
    /// session rides into the new one.
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.lines = RefCell::new(Default::default());
        self.slots.reset();
    }
}

impl PreparedFrame {
    /// Drop the last draw pass so the new session recomputes the first
    /// frame rather than reusing stale measurements. Feedback stays: each
    /// draw writes the dimensions its own path needs, so the value a reader
    /// sees belongs to the frame that was drawn. The caches are the
    /// Renderer's and are cleared there.
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.layout = Default::default();
        self.keys = Default::default();
    }
}
