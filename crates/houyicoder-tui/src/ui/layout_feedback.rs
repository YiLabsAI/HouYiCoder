//! The renderer's layout feedback: the terminal height, column count, and
//! transcript pane width the draw pass measured, plus the native cursor
//! position it parked. Each is an interior-mutable cell the count, layout, and
//! input paths read back. One owner because all of them share one provenance:
//! they are written by the draw pass and read by code that needs the render's
//! own numbers, so they travel as the render's measured feedback rather than
//! loose App fields.

use std::cell::Cell;

/// The last-measured layout the renderer fed back: the terminal height (for
/// height-aware checklist rendering), the transcript pane width (so the count
/// path soft-wraps to the same width the render used), the terminal column
/// count the input wrap used (so key navigation walks the same wrap), and the
/// native cursor position an input method parked (so the next draw restores
/// it). Each is a Cell mirror the draw pass writes and the rest of the app
/// reads.
#[derive(Debug)]
pub(crate) struct LayoutFeedback {
    /// Last terminal height seen by the draw pass. 24 before the first render
    /// so height-aware rendering has a sane initial guess.
    pub(crate) terminal_rows: Cell<u16>,
    /// Last transcript pane width (inner area width, not the input wrap
    /// column). 0 before the first render means do-not-wrap.
    pub(crate) transcript_width: Cell<u16>,
    /// Last terminal column count the input wrap used, set by the draw pass
    /// and read by key navigation to move up/down and home/end through the
    /// same wrap. 80 before the first render as the conventional width.
    pub(crate) terminal_cols: Cell<usize>,
    /// Hidden hardware cursor position an input method parked, written by the
    /// input-bar draw and read back so the next draw restores it exactly.
    /// None when no input method has moved the cursor.
    pub(crate) native_cursor_position: Cell<Option<(u16, u16)>>,
}

impl Default for LayoutFeedback {
    fn default() -> Self {
        Self {
            terminal_rows: Cell::new(24),
            transcript_width: Cell::new(0),
            terminal_cols: Cell::new(80),
            native_cursor_position: Cell::new(None),
        }
    }
}
