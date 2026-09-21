//! Memory-change notice rows: a System line rendered as a foldable block whose
//! summary is the first logical row and whose per-key rows are the detail, so
//! several changes (or one) collapse to the summary instead of flooding the
//! transcript. Ctrl+O and a click toggle it via the group key, like a tool-call
//! group.

use crate::fold::notice_lines;
use crate::records::TranscriptLine;
use crate::state::App;
use crate::view::working::row_buffer::{Row, RowBuffer};

/// Render one memory-change notice block into the row buffer. The rows come
/// from the fold layer's notice_lines, which the fold-aware count also walks,
/// so a summary long enough to wrap reserves the rows it draws. Every row
/// carries the group key so Ctrl+O and a click in the block toggle the same
/// notice.
pub(super) fn push_notice_rows(
    app: &App,
    idx: usize,
    key: &str,
    expanded: bool,
    width: u16,
    sink: &mut RowBuffer,
) {
    if !sink.is_empty() {
        sink.push(Row::spacer());
    }
    let Some(TranscriptLine::System(text)) = app.active_transcript().get(idx) else {
        return;
    };
    for row in notice_lines(text, expanded, width as usize) {
        sink.push(
            Row::new(crate::selection::TAG_FOLD, row)
                .fold_key(Some(key.to_string()))
                .group(Some(key.to_string())),
        );
    }
}
