//! The renderer's layout feedback: the last terminal height and transcript
//! pane width the draw pass measured, stashed as interior-mutable cells the
//! count and layout paths read back. One owner because the two dims share one
//! provenance: both are written by the draw pass and read by the count and
//! layout math, so they travel as the render's measured bounds rather than
//! two loose cells.

use std::cell::Cell;

/// The last-measured layout dimensions the renderer fed back: the terminal
/// height (for height-aware checklist rendering) and the transcript pane
/// width (so the count path soft-wraps to the same width render used). Both
/// are Cell mirrors the draw pass writes and the count and layout paths read.
#[derive(Debug)]
pub(crate) struct LayoutFeedback {
    /// Last terminal height seen by the draw pass. 24 before the first
    /// render so height-aware rendering has a sane initial guess.
    pub(crate) terminal_rows: Cell<u16>,
    /// Last transcript pane width (inner area width, not the input wrap
    /// column). 0 before the first render means do-not-wrap.
    pub(crate) transcript_width: Cell<u16>,
}

impl Default for LayoutFeedback {
    fn default() -> Self {
        Self {
            terminal_rows: Cell::new(24),
            transcript_width: Cell::new(0),
        }
    }
}
