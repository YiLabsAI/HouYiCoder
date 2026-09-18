//! Memory-change notice rows: a System line rendered as a foldable block whose
//! summary is the first logical row and whose per-key rows are the detail, so
//! several changes (or one) collapse to the summary instead of flooding the
//! transcript. Ctrl+O and a click toggle it via the group key, like a tool-call
//! group.

use crate::records::TranscriptLine;
use crate::state::App;
use crate::view::working::row_buffer::{Row, RowBuffer};

/// Render one memory-change notice block into the row buffer. Collapsed shows
/// the summary row (a collapse handle); expanded shows the summary then each
/// changed key, each wrapped to the pane width. Every row carries the group
/// key so Ctrl+O and a click in the block toggle the same notice.
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
    if expanded {
        let mut is_summary = true;
        for logical in text.split('\n') {
            // Prefix the summary row before wrapping, exactly as the count
            // path does, so a summary near a wrap boundary reserves the same
            // rows the renderer emits (count==render).
            let logical = if is_summary {
                format!("✻ {logical}")
            } else {
                logical.to_string()
            };
            is_summary = false;
            for row in crate::view::line_wrap::wrap_line(&logical, width as usize) {
                sink.push(
                    Row::new(crate::selection::TAG_SYSTEM, row)
                        .fold_key(Some(key.to_string()))
                        .group(Some(key.to_string())),
                );
            }
        }
    } else {
        let first = text.split('\n').next().unwrap_or("");
        sink.push(
            Row::new(
                crate::selection::TAG_SYSTEM,
                format!("✻ {first} (ctrl+o to expand)"),
            )
            .fold_key(Some(key.to_string()))
            .group(Some(key.to_string())),
        );
    }
}
