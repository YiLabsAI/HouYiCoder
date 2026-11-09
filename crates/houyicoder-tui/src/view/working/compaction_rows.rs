//! The compaction row: one foldable line for a context compaction.
//!
//! Collapsed it names the event and the chord that reveals the digest;
//! expanded it keeps the head and prints the digest under it. Keeping the
//! digest behind a fold is the point: it is reference material the reader
//! consults on demand, and printing it in flow spends the screen on it.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::records::compaction_row_text;
use crate::rows::row_set::{Row, RowSet};
use crate::selection::{TAG_FOLD, TAG_SYSTEM};
use crate::state::App;
use crate::toggle_hint::ToggleHint;
use crate::view::line_wrap::wrap_line;

/// Maximum wrapped lines rendered when a compaction digest is expanded inline.
pub(crate) const MAX_EXPANDED_COMPACTION_ROWS: usize = 40;

/// Filter out internal machine-only markers from a compaction summary so the
/// expanded transcript block reads cleanly for humans.
pub(crate) fn visible_digest_lines(summary: &str) -> Vec<&str> {
    let has_structured = summary.contains("Files touched:")
        || summary.contains("Test results:")
        || summary.contains("Todo snapshot:");
    summary
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed == "--- derived-from-log (authoritative) ---"
                || trimmed.starts_with("Derivation: event=")
                || trimmed.starts_with("Continuation: Pending todo items remain above.")
            {
                return false;
            }
            if has_structured && trimmed.starts_with("Earlier conversation context (") {
                return false;
            }
            true
        })
        .collect()
}

/// Push the compaction row and, when it is expanded, the digest under it.
///
/// The row is the only trace of the event, so it carries the fold affordance
/// every other foldable block carries: the shared chord toggles it and a click
/// on the row does the same. Collapsed it is one dim line; expanded it stays
/// the head and the summary follows as ordinary text, wrapped to the width so
/// a long digest cannot overflow the frame.
pub(crate) fn push_compaction_rows(
    key: &str,
    stats: &str,
    summary: &str,
    width: u16,
    app: &App,
    sink: &mut RowSet,
) {
    let expanded = app.ui.transcript_view.expanded.fold_groups.contains(key) || app.ui.verbose;
    // A row whose key is empty came from a digest with no boundary. No fold
    // group owns it, so it is drawn as a plain line: offering the chord would
    // promise a toggle that has nothing to act on.
    let foldable = !key.is_empty();
    let hint = if !foldable {
        None
    } else if expanded {
        Some(ToggleHint::Collapse)
    } else {
        Some(ToggleHint::Expand)
    };
    let head = compaction_row_text(stats, hint);
    let head_tag = if foldable { TAG_FOLD } else { TAG_SYSTEM };
    let row = Row::new(head_tag, head.clone()).pre(Some(Line::from(Span::styled(
        head,
        Style::default().fg(Color::DarkGray),
    ))));
    sink.push(if foldable {
        row.fold_key(Some(key.to_string()))
            .group(Some(key.to_string()))
    } else {
        row
    });
    let lines = visible_digest_lines(summary);
    if !expanded || lines.is_empty() {
        return;
    }
    let mut count = 0;
    for line in lines {
        for wrapped in wrap_line(line, width as usize) {
            count += 1;
            if count > MAX_EXPANDED_COMPACTION_ROWS {
                sink.push(
                    Row::new(
                        TAG_SYSTEM,
                        "  └ … and more (searchable via /search)".to_string(),
                    )
                    .group(Some(key.to_string()))
                    .pre(Some(Line::from(Span::styled(
                        "  └ … and more (searchable via /search)",
                        Style::default().fg(Color::DarkGray),
                    )))),
                );
                return;
            }
            sink.push(
                Row::new(TAG_SYSTEM, wrapped.to_string())
                    .group(Some(key.to_string()))
                    .pre(Some(Line::from(Span::styled(
                        wrapped.to_string(),
                        Style::default().fg(Color::DarkGray),
                    )))),
            );
        }
    }
}
