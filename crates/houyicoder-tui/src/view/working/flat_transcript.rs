//! Flat transcript rendering for the byte-window search view. Matches
//! draw_transcript's per-line rendering (the row layer) but skips the slot
//! layer (display_slots + fold grouping): a byte window materializes one
//! screen at a time, so there is no whole-vec fold object to collapse and no
//! collapse handle that could point at a global row. The slot layer's job is
//! absent here, not duplicated -- this is the correct model for a windowed
//! view, not a parallel render path. Row-layer rendering (highlight, result
//! body, diff, the spacer logic) is shared with the live path via
//! push_line_rows + highlighted_line.
//!
//! Owns its own scroll state (window_scroll) and publishes its own row count
//! there, so it never touches TranscriptScroll/display_slots/total or the five
//! consumers that read them. The flat count==render pair (flat_display_rows +
//! flat_row_of_line) walks the same lines + spacer rule this render path emits.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use crate::records::ToolOutcome;
use crate::state::App;
use crate::view::markers::{diff_row, styled_row};
use crate::view::working::working_transcript::{highlighted_line, push_line_rows};

/// The flat window render: walk the active transcript (the loaded window's
/// lines) directly, emit one row set, publish window_scroll, slice the
/// viewport, apply search highlighting. No live tail (the window is a frozen
/// snapshot of the log -- no spinner, no streaming assistant row, no checklist
/// tail; those belong to the live Working surface).
//
// too_many_lines + cognitive_complexity: this is a render loop -- emit
// rows, publish scroll state, slice, highlight. Splitting it (row-build vs
// highlight) would scatter the count==render invariant across two functions
// and make the flat-walk parity harder to hold. It is the simplified isomorph
// of draw_transcript (same shape minus the fold slot layer); keep them
// structurally parallel.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub(super) fn draw_flat_transcript(f: &mut Frame, area: Rect, app: &App) {
    // Pump one index chunk per frame while the G full-scan builds (keeps the
    // UI responsive + lets Esc interrupt). Done before rendering so the
    // progress cells the status bar reads are current for this frame.
    app.pump_index_chunk();
    const PLAIN: u8 = crate::selection::TAG_PLAIN;
    const USER: u8 = crate::selection::TAG_USER;
    const SYSTEM: u8 = crate::selection::TAG_SYSTEM;
    const DIFF_ADD: u8 = crate::selection::TAG_DIFF_ADD;
    const DIFF_DEL: u8 = crate::selection::TAG_DIFF_DEL;
    const DIFF_HUNK: u8 = crate::selection::TAG_DIFF_HUNK;
    const DIFF_CTX: u8 = crate::selection::TAG_DIFF_CTX;
    let mut rows: Vec<(u8, String, Option<ToolOutcome>)> = Vec::new();
    let mut row_callids: Vec<Option<String>> = Vec::new();
    let mut fold_keys: Vec<Option<String>> = Vec::new();
    let mut expanded_group: Vec<Option<String>> = Vec::new();
    let mut turn_ids: Vec<Option<String>> = Vec::new();
    let mut pre_rendered: Vec<Option<Line<'static>>> = Vec::new();
    // No slot layer: each line is its own row set. grp is None -- the window
    // has no fold groups, so expanded_group stays None throughout.
    for line in app.active_transcript() {
        push_line_rows(
            line,
            None,
            area.width,
            app,
            &mut rows,
            &mut row_callids,
            &mut fold_keys,
            &mut expanded_group,
            &mut turn_ids,
            &mut pre_rendered,
        );
    }
    let cap = area.height as usize;
    app.window_scroll.cap.set(cap);
    let total = rows.len();
    app.window_scroll.total.set(total);
    let top = app.window_scroll.top_offset();
    // Stash the full + visible row sets so copy can extract selected text from
    // the window (selection past the visible edge, viewport scrolled between
    // draw and copy). These stashes are the rendered-row cache, not
    // TranscriptScroll -- distinct from the five total consumers.
    *app.last_all_rows.borrow_mut() = rows.iter().map(|(t, s, _)| (*t, s.clone())).collect();
    let visible: Vec<(u8, String, Option<ToolOutcome>)> =
        rows.into_iter().skip(top).take(cap).collect();
    let visible_callids: Vec<Option<String>> =
        row_callids.into_iter().skip(top).take(cap).collect();
    let visible_fold_keys: Vec<Option<String>> =
        fold_keys.into_iter().skip(top).take(cap).collect();
    let visible_expanded_group: Vec<Option<String>> =
        expanded_group.into_iter().skip(top).take(cap).collect();
    let visible_turn_ids: Vec<Option<String>> = turn_ids.into_iter().skip(top).take(cap).collect();
    let visible_pre: Vec<Option<Line<'static>>> =
        pre_rendered.into_iter().skip(top).take(cap).collect();

    let inner = area;
    app.transcript_rect.set(inner);
    // The count path (flat_row_of_line) soft-wraps to the same width the
    // render path just used -- count == render holds for the window too.
    app.last_transcript_width.set(inner.width);
    *app.last_transcript_rows.borrow_mut() =
        visible.iter().map(|(t, s, _)| (*t, s.clone())).collect();
    *app.last_row_callids.borrow_mut() = visible_callids;
    *app.last_row_fold_keys.borrow_mut() = visible_fold_keys;
    *app.last_row_expanded_group.borrow_mut() = visible_expanded_group;
    *app.last_row_turn_ids.borrow_mut() = visible_turn_ids;
    f.render_widget(Block::default().borders(Borders::NONE), area);

    let q = if app.search.active {
        app.search.query.trim().to_ascii_lowercase()
    } else {
        String::new()
    };
    // The focused match's line spans multiple screen rows in verbose (a tool
    // body expands). Mark [start, start+rows) current so highlighted_line
    // paints yellow on every query occurrence in the focused line. Flat row
    // math (flat_row_of_line + line_display_rows), not the fold path.
    let focused_range = if app.search.active && !q.is_empty() {
        app.search.focused_line().map(|i| {
            let start = app.flat_row_of_line(i);
            let rows = app.line_display_rows(&app.active_transcript()[i]);
            start..start + rows
        })
    } else {
        None
    };
    let user_bg = Style::new().bg(Color::Indexed(238));
    let dim = Style::new().fg(Color::DarkGray);
    let lines: Vec<Line> = visible
        .iter()
        .zip(visible_pre)
        .enumerate()
        .map(|(idx, ((tag, r, _), pre))| {
            let row = top + idx;
            let is_current = focused_range
                .as_ref()
                .is_some_and(|rng| rng.start <= row && row < rng.end);
            match pre {
                Some(line) => {
                    if q.is_empty() {
                        line
                    } else {
                        highlighted_line(r, &q, is_current)
                    }
                }
                None => match *tag {
                    USER => highlighted_line(r, &q, is_current).style(user_bg),
                    SYSTEM => highlighted_line(r, &q, is_current).style(dim),
                    DIFF_ADD | DIFF_DEL | DIFF_HUNK | DIFF_CTX => {
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
}
