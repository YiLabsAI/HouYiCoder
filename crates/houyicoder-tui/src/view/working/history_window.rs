//! Rendering for a bounded history window loaded from a large session log.
//!
//! The window has no global fold groups because only one byte range is loaded.
//! Per-line styling and spacing are shared with the main transcript renderer.
//!
//! The within-window scroll owns its row count and position independently
//! from the active conversation.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use crate::rows::row_set::RowSet;
use crate::selection::{
    TAG_DIFF_ADD, TAG_DIFF_CTX, TAG_DIFF_DEL, TAG_DIFF_HUNK, TAG_SYSTEM, TAG_USER,
};
use crate::state::App;
use crate::view::markers::{diff_row, styled_row};
use crate::view::working::transcript::{
    highlighted_line, push_line_rows, refresh_wide_cells, user_row,
};

/// Render the loaded history range with independent scrolling and search
/// highlighting. Dynamic conversation-tail rows are intentionally absent.
pub(super) fn draw_history_window(f: &mut Frame, area: Rect, app: &App) {
    // The scan advances on the loop, not here: a chunk reads the log.
    // This path only renders the progress cells the loop published.
    let mut sink = RowSet::default();
    // No slot layer: each line is its own row set. grp is None -- the window
    // has no fold groups, so the enclosing group stays None throughout.
    for line in app.active_transcript() {
        push_line_rows(line, None, area.width, app, &mut sink);
    }
    let cap = area.height as usize;
    app.window_scroll().cap.set(cap);
    let total = sink.len();
    app.window_scroll().total.set(total);
    let top = app.window_scroll().top_offset();
    // Stash the full + visible row sets so copy can extract selected text from
    // the window (selection past the visible edge, viewport scrolled between
    // draw and copy). These stashes are the rendered-row cache, not
    // TranscriptScroll -- distinct from the five total consumers.
    *app.ui.frame.layout.last_all_rows.borrow_mut() = sink.text_rows();
    // One traversal slices the rows and the metadata by the same bounds, so
    // the visible window cannot give a row its neighbour's keys.
    let visible = sink.slice(top, cap);

    let inner = area;
    app.ui.frame.layout.transcript_rect.set(inner);
    // The count path (history_row_of_line) soft-wraps to the same width the
    // render path just used -- count == render holds for the window too.
    app.ui.frame.feedback.transcript_width.set(inner.width);
    *app.ui.frame.layout.last_transcript_rows.borrow_mut() = visible.text_rows();
    app.ui.frame.keys.borrow_mut().replace(&visible);
    f.render_widget(Block::default().borders(Borders::NONE), area);

    let q = if app.ui.transcript_view.search_active() {
        app.ui
            .transcript_view
            .search_query()
            .trim()
            .to_ascii_lowercase()
    } else {
        String::new()
    };
    // The focused match's line spans multiple screen rows in verbose (a tool
    // body expands). Mark [start, start+rows) current so highlighted_line
    // paints yellow on every query occurrence in the focused line. Flat row
    // math (history_row_of_line + line_display_rows), not the fold path.
    let focused_range = if app.ui.transcript_view.search_active() && !q.is_empty() {
        app.ui.transcript_view.search_focused_line().map(|i| {
            let start = app.history_row_of_line(i);
            let rows = app.line_display_rows(&app.active_transcript()[i]);
            start..start + rows
        })
    } else {
        None
    };
    let dim = Style::new().fg(Color::DarkGray);
    let lines: Vec<Line> = visible
        .rows()
        .iter()
        .zip(visible.pre_rendered())
        .enumerate()
        .map(|(idx, ((tag, r, _), pre))| {
            let row = top + idx;
            let is_current = focused_range
                .as_ref()
                .is_some_and(|rng| rng.start <= row && row < rng.end);
            match pre {
                Some(line) => {
                    if q.is_empty() {
                        line.clone()
                    } else {
                        highlighted_line(r, &q, is_current)
                    }
                }
                None => match *tag {
                    TAG_USER => user_row(r, &q, is_current, inner.width),
                    TAG_SYSTEM => highlighted_line(r, &q, is_current).style(dim),
                    TAG_DIFF_ADD | TAG_DIFF_DEL | TAG_DIFF_HUNK | TAG_DIFF_CTX => {
                        diff_row(r, *tag, inner.width, None)
                    }
                    _ => {
                        if q.is_empty() {
                            styled_row(r, None)
                                .unwrap_or_else(|| highlighted_line(r, &q, is_current))
                        } else {
                            highlighted_line(r, &q, is_current)
                        }
                    }
                },
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).style(Style::default().fg(Color::Reset).bg(Color::Reset)),
        inner,
    );
    refresh_wide_cells(f, inner);
}
