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
