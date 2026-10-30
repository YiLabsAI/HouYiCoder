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
use crate::selection::TAG_SYSTEM;
use crate::state::App;
use crate::toggle_hint::ToggleHint;
use crate::view::line_wrap::wrap_line;

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
    let expanded = app.ui.transcript_view.expanded.fold_groups.contains(key);
    let hint = if expanded {
        ToggleHint::Collapse
    } else {
        ToggleHint::Expand
    };
    let head = compaction_row_text(stats, hint);
    sink.push(
        Row::new(TAG_SYSTEM, head.clone())
            .fold_key(Some(key.to_string()))
            .group(Some(key.to_string()))
            .pre(Some(Line::from(Span::styled(
                head,
                Style::default().fg(Color::DarkGray),
            )))),
    );
    if !expanded || summary.is_empty() {
        return;
    }
    for line in summary.lines() {
        for wrapped in wrap_line(line, width as usize) {
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
