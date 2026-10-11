//! The draw pass's state: the per-line render cache and the last frame's
//! measured layout, rows, and keys.

use std::cell::RefCell;

use crate::display_rows_cache::DisplayRowsCache;
use crate::last_row_keys::LastRowKeys;
use crate::layout_feedback::LayoutFeedback;
use crate::layout_snapshot::LayoutSnapshot;
use crate::render_cache::RenderCache;

/// The reusable per-line render cache. A line parsed at one width or
/// expansion is reused across frames and transcript rebuilds; a session
/// switch clears it. The RefCell keeps it writeable through the draw pass,
/// which receives an immutable App.
#[derive(Default)]
pub(crate) struct Renderer {
    pub(crate) cache: RefCell<RenderCache>,
}

/// The last draw pass's render output: the measured region rectangles, the
/// feedback dimensions the count and layout paths read back, the pre-rendered
/// rows cache, and the per-row keys the click and Ctrl+O paths resolve
/// against. Written together by the draw pass and read together by every
/// hit-test and selection path.
#[derive(Default)]
pub(crate) struct PreparedFrame {
    pub(crate) layout: LayoutSnapshot,
    pub(crate) feedback: LayoutFeedback,
    pub(crate) rows: DisplayRowsCache,
    pub(crate) keys: LastRowKeys,
}

impl Renderer {
    /// Clear the per-line render cache so a line parsed for the old session
    /// does not ride into the new one at a different width or expansion.
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.cache = RefCell::new(Default::default());
    }
}

impl PreparedFrame {
    /// Drop the last draw pass so the new session recomputes the first
    /// frame rather than reusing stale measurements. Feedback stays — it
    /// is rewritten each draw, so a stale value self-corrects rather than
    /// layering over the new session.
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.layout = Default::default();
        self.rows = Default::default();
        self.rows.version.set(u64::MAX);
        self.keys = Default::default();
    }
}
