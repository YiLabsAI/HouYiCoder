//! Rows appended after stable transcript history: streaming assistant text,
//! the activity indicator, and the checklist. This bounded tail is rebuilt on
//! every draw.

use ratatui::layout::Rect;

use crate::rows::row_set::{Row, RowSet};
use crate::selection::TAG_FOLD;
use crate::selection::TAG_PLAIN;
use crate::selection::TAG_SPINNER;
use crate::state::App;
use crate::view::spinner::spinner_row_text;
use crate::view::todo_list::TODO_FOLD_KEY;
use crate::view::todo_list::collapsed_summary_row;
use crate::view::todo_list::render_rows;

/// Build the per-frame live rows: the streaming assistant text (when active),
/// the spinner (when a run is in flight), and the session checklist. Each
/// section gets a leading blank spacer when there is content above it (the
/// slots cache or a prior live section) so sections do not run together. The
/// has_slots flag threads whether the cached slots are non-empty so the first
/// spacer guard works before any live row exists.
pub(super) fn build_tail_rows(area: Rect, app: &App, has_slots: bool) -> RowSet {
    const PLAIN: u8 = TAG_PLAIN;
    const SPINNER: u8 = TAG_SPINNER;

    let mut sink = RowSet::default();
    // Each section is preceded by a spacer when anything sits above it,
    // whether that is the slots cache or an earlier live section.
    let spacer_if_needed = |sink: &mut RowSet| {
        if has_slots || !sink.is_empty() {
            sink.push(Row::spacer());
        }
    };

    // The parent's live streaming text + spinner belong to the parent
    // view. Suppress them while a teammate view is open so the child's
    // transcript renders alone — the parent is not talking to the user
    // while they are viewing a child.
    if app.ui.teammate_view.is_some() {
        return RowSet::default();
    }

    if let Some(p) = app.run_progress()
        && p.live_active
        && !p.live_assistant_text.is_empty()
    {
        spacer_if_needed(&mut sink);
        let (md_lines, md_plain) = app
            .ui
            .renderer
            .lines
            .borrow_mut()
            .live_agent_rows(p.live_assistant_text.as_str(), area.width);
        for (md_line, plain) in md_lines.into_iter().zip(md_plain) {
            sink.push(Row::new(PLAIN, plain).pre(Some(md_line)));
        }
    }

    if app.agent_busy()
        && let Some(start) = app.run_started()
    {
        spacer_if_needed(&mut sink);
        let text = spinner_row_text(app, start.elapsed(), area.width);
        sink.push(Row::new(SPINNER, text));
    }

    let todo_rows = render_rows(app);
    if !todo_rows.is_empty() {
        spacer_if_needed(&mut sink);
        // The collapsed summary is the row that offers the hidden items, so it
        // carries the fold affordance every other foldable block carries: the
        // shared chord toggles it, and a click on the row does the same. A
        // summary with no affordance is a line telling the reader there is
        // more to see and no way to see it.
        let summary = collapsed_summary_row(&todo_rows);
        for (i, (plain, styled)) in todo_rows.into_iter().enumerate() {
            // The summary is tagged as a fold row, which is the tag the click
            // hit-test reads to decide between opening a fold and opening a
            // delegation. A plain tag would route the click to the delegation
            // path and the checklist would stay shut.
            let row = if Some(i) == summary {
                Row::new(TAG_FOLD, plain)
                    .pre(Some(styled))
                    .fold_key(Some(TODO_FOLD_KEY.to_string()))
                    .group(Some(TODO_FOLD_KEY.to_string()))
            } else {
                Row::new(PLAIN, plain).pre(Some(styled))
            };
            sink.push(row);
        }
    }

    sink
}
