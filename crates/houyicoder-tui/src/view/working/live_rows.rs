//! The per-frame live rows: streaming assistant text, the spinner, and the
//! todo checklist. Built fresh every frame (cheap, bounded); distinct from
//! the slots cache (the stable transcript rows). Extracted from
//! working_transcript.rs on size grounds.

use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::records::ToolOutcome;
use crate::state::App;

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

    let mut rows: Vec<(u8, String, Option<ToolOutcome>)> = Vec::new();
    let mut callids: Vec<Option<String>> = Vec::new();
    let mut fold_keys: Vec<Option<String>> = Vec::new();
    let mut expanded_group: Vec<Option<String>> = Vec::new();
    let mut turn_ids: Vec<Option<String>> = Vec::new();
    let mut pre_rendered: Vec<Option<Line<'static>>> = Vec::new();

    if app.live_active && !app.live_assistant_text.is_empty() {
        if has_slots || !rows.is_empty() {
            rows.push((PLAIN, String::new(), None));
            callids.push(None);
            fold_keys.push(None);
            expanded_group.push(None);
            turn_ids.push(None);
            pre_rendered.push(None);
        }
        let (md_lines, md_plain) = app
            .render_cache
            .borrow_mut()
            .live_agent_rows(&app.live_assistant_text, area.width);
        for (md_line, plain) in md_lines.into_iter().zip(md_plain) {
            rows.push((PLAIN, plain, None));
            callids.push(None);
            fold_keys.push(None);
            expanded_group.push(None);
            turn_ids.push(None);
            pre_rendered.push(Some(md_line));
        }
    }

    if app.agent_busy
        && let Some(start) = app.run_started
    {
        if has_slots || !rows.is_empty() {
            rows.push((PLAIN, String::new(), None));
            callids.push(None);
            fold_keys.push(None);
            expanded_group.push(None);
            turn_ids.push(None);
            pre_rendered.push(None);
        }
        let text = crate::view::spinner::spinner_row_text(app, start.elapsed(), area.width);
        rows.push((SPINNER, text, None));
        callids.push(None);
        fold_keys.push(None);
        expanded_group.push(None);
        turn_ids.push(None);
        pre_rendered.push(None);
    }

    let todo_rows = crate::view::todo_list::render_rows(app);
    if !todo_rows.is_empty() {
        if has_slots || !rows.is_empty() {
            rows.push((PLAIN, String::new(), None));
            callids.push(None);
            fold_keys.push(None);
            expanded_group.push(None);
            turn_ids.push(None);
            pre_rendered.push(None);
        }
        for (plain, styled) in todo_rows {
            rows.push((PLAIN, plain, None));
            callids.push(None);
            fold_keys.push(None);
            expanded_group.push(None);
            turn_ids.push(None);
            pre_rendered.push(Some(styled));
        }
    }

    let all_rows: Vec<(u8, String)> = rows.iter().map(|(t, s, _)| (*t, s.clone())).collect();
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
