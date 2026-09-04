//! The per-frame live rows: streaming assistant text, the spinner, and the
//! todo checklist. Built fresh every frame (cheap, bounded); distinct from
//! the slots cache (the stable transcript rows). Extracted from
//! working_transcript.rs on size grounds.

use ratatui::layout::Rect;
use ratatui::text::Line;

use super::row_sink::{Row, RowSink};
use crate::records::ToolOutcome;
use crate::state::App;

#[derive(Default)]
pub(super) struct LiveRows {
    pub rows: Vec<(u8, String, Option<ToolOutcome>)>,
    pub callids: Vec<Option<String>>,
    pub fold_keys: Vec<Option<String>>,
    pub expanded_group: Vec<Option<String>>,
    pub turn_ids: Vec<Option<String>>,
    pub pre_rendered: Vec<Option<Line<'static>>>,
    pub all_rows: Vec<(u8, String)>,
}

/// Build the per-frame live rows: the streaming assistant text (when active),
/// the spinner (when a run is in flight), and the session checklist. Each
/// section gets a leading blank spacer when there is content above it (the
/// slots cache or a prior live section) so sections do not run together. The
/// has_slots flag threads whether the cached slots are non-empty so the first
/// spacer guard works before any live row exists.
pub(super) fn build_live_rows(area: Rect, app: &App, has_slots: bool) -> LiveRows {
    const PLAIN: u8 = crate::selection::TAG_PLAIN;
    const SPINNER: u8 = crate::selection::TAG_SPINNER;

    let mut sink = RowSink::default();
    // Each section is preceded by a spacer when anything sits above it,
    // whether that is the slots cache or an earlier live section.
    let spacer_if_needed = |sink: &mut RowSink| {
        if has_slots || !sink.is_empty() {
            sink.push(Row::spacer());
        }
    };

    // The parent's live streaming text + spinner belong to the parent
    // view. Suppress them while a teammate view is open so the child's
    // transcript renders alone — the parent is not talking to the user
    // while they are viewing a child.
    if app.teammate_view.is_some() {
        return LiveRows::default();
    }

    if app.live_active && !app.live_assistant_text.is_empty() {
        spacer_if_needed(&mut sink);
        let (md_lines, md_plain) = app
            .render_cache
            .borrow_mut()
            .live_agent_rows(&app.live_assistant_text, area.width);
        for (md_line, plain) in md_lines.into_iter().zip(md_plain) {
            sink.push(Row::new(PLAIN, plain).pre(Some(md_line)));
        }
    }

    if app.agent_busy
        && let Some(start) = app.run_started
    {
        spacer_if_needed(&mut sink);
        let text = crate::view::spinner::spinner_row_text(app, start.elapsed(), area.width);
        sink.push(Row::new(SPINNER, text));
    }

    let todo_rows = crate::view::todo_list::render_rows(app);
    if !todo_rows.is_empty() {
        spacer_if_needed(&mut sink);
        for (plain, styled) in todo_rows {
            sink.push(Row::new(PLAIN, plain).pre(Some(styled)));
        }
    }

    let all_rows = sink.text_rows();
    let (rows, callids, fold_keys, expanded_group, turn_ids, pre_rendered) = sink.into_parts();
    LiveRows {
        rows,
        callids,
        fold_keys,
        expanded_group,
        turn_ids,
        pre_rendered,
        all_rows,
    }
}
