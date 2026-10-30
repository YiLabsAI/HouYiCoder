//! /status pane content: renders the Status / Config / Usage sub-tabs into
//! the shared Pane template below the transcript tail. The Status tab's field
//! logic is shared with render_status (the String path the stub /status and
//! the unit tests exercise); this module wraps those lines in a sub-tab header
//! + a footer so the pane is a live surface, not a transcript dump. A
//! Settings-modal-style tabbed status minus the Stats tab. The sub-tab
//! cycles with the Tab, Left, or Right keys.
#![allow(clippy::doc_lazy_continuation)]

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::command::render::{field, render_status};
use crate::state::{App, enums::StatusTab};
use crate::view::navigation::{key_hint, tab_header_at};
use crate::view::status::config_rows::{CONFIG_ROWS, row_changes, row_label, row_value};

/// Default height /status asks for: a header + up to ~12 status lines + a
/// footer. Capped at half the main area by draw_command_pane.
pub(crate) const STATUS_PANE_HEIGHT: u16 = 20;
const STATUS_TABS: [(StatusTab, &str); 3] = [
    (StatusTab::Status, "Status"),
    (StatusTab::Config, "Config"),
    (StatusTab::Usage, "Usage"),
];

/// Render the /status content into the Pane inner rect (the closure passed
/// to draw_command_pane). Sub-tab header + the active tab's content + footer.
pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    // The tab row, a blank spacer, the body, another spacer, and the key
    // hint. The spacers are what keep the three regions reading as three:
    // without them the first field row sits against the tab labels and the
    // hint sits against the last field, and the whole pane reads as one wall.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    // The tab row starts at the left edge because the body does: every field
    // below begins in column zero, so an indented tab row would be the one
    // line out of alignment.
    f.render_widget(
        Paragraph::new(tab_header_at(
            app.ui.navigation.status_pane.tab,
            &STATUS_TABS,
            "",
        )),
        chunks[0],
    );
    // Body: the active tab's content.
    draw_tab_body(f, app.ui.navigation.status_pane.tab, chunks[2], app);
    let footer = Paragraph::new(key_hint(&footer_keys(app)));
    f.render_widget(footer, chunks[4]);
}

/// The keys the pane offers, which follow the tab: a list tab names the two
/// keys that move and act on its cursor, and the reading tabs name the tab
/// switch alone.
fn footer_keys(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut keys: Vec<(&'static str, &'static str)> = Vec::new();
    if app.ui.navigation.status_pane.tab == StatusTab::Config {
        keys.push(("Up/Down", "select"));
        keys.push(("Enter", "change"));
    }
    keys.push(("Tab/Left/Right", "switch tab"));
    keys.push(("Esc", "close"));
    keys
}

/// The body of the active sub-tab. Status reuses render_status (the shared
/// String path + its unit-test authority); Config shows the sandbox / mode /
/// model configuration; Usage shows the token breakdown. When the user is
/// editing the session name on the Status tab, the name row is spliced into
/// an editable line with an inverted caret (the rest of the body is static).
fn draw_tab_body(f: &mut Frame, tab: StatusTab, area: Rect, app: &App) {
    let body: String = match tab {
        StatusTab::Status => {
            let snap = app.snapshot_or_stub();
            render_status(&snap, &app.sessions.current.identity.session_id)
        }
        StatusTab::Config => render_config(app),
        StatusTab::Usage => render_usage(app),
    };
    let mut lines: Vec<Line<'static>> = body.lines().map(|l| Line::from(l.to_string())).collect();
    // Splice the name row on the Status tab: an inline hint when browsing
    // (e to rename), an editable line with a caret when editing. The name
    // line is the one whose label starts with "Session name"; finding it
    // (rather than assuming a position) stays robust to render_status
    // reordering its rows.
    if tab == StatusTab::Status
        && let Some(idx) = lines.iter().position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_start()
                .starts_with("Session name")
        })
    {
        match app.ui.navigation.status_pane.name_edit.as_ref() {
            // Editing: replace the row with the editable caret line + a hint.
            Some(field) => lines[idx] = name_edit_line(field),
            // Browsing: append a dim "e to rename" hint to the static row.
            None => lines[idx] = name_hint_line(take(&mut lines[idx])),
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Append a dim "e to rename" hint to the static name row so the user knows
/// the inline-edit affordance exists without a separate footer line.
fn name_hint_line(mut row: Line<'static>) -> Line<'static> {
    row.spans.push(Span::styled(
        "  (e to rename)".to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    row
}

/// The editable session-name row: the label + the buffer with an inverted
/// caret at the cursor (a grapheme under the cursor, or a trailing space
/// block when the cursor is past the end). Matches the input-bar caret style.
fn name_edit_line(field: &InputField) -> Line<'static> {
    // Label matches render::field's "{:<22}" so the caret row does not jump
    // left/right against the static rows when editing starts/stops.
    let label: Span<'static> = Span::raw(format!("{:<22}", "Session name:"));
    let body_style = Style::new().fg(Color::Reset);
    let cursor_style = Style::new().bg(Color::White).fg(Color::Black);
    let text = field.value();
    let cursor = field.cursor().min(text.len());
    let before = &text[..cursor];
    let rest = &text[cursor..];
    let mut spans: Vec<Span<'static>> = vec![label];
    if !before.is_empty() {
        spans.push(Span::styled(before.to_string(), body_style));
    }
    if let Some(g) = rest.graphemes(true).next() {
        spans.push(Span::styled(g.to_string(), cursor_style));
        let after = &rest[g.len()..];
        if !after.is_empty() {
            spans.push(Span::styled(after.to_string(), body_style));
        }
    } else {
        spans.push(Span::styled(" ".to_string(), cursor_style));
    }
    spans.push(Span::styled(
        "  (Enter save · Esc cancel)".to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

/// The Config tab: runtime configuration knobs -- model, permission mode,
/// sandbox, breaker, + the settings-file memory toggles. Reads live app
/// state + the snapshot's toggle fields so the tab is a focused config view.
/// Display-only: the user flips a toggle by editing the settings file (the
/// settings file is the source of truth, edited externally, not inline).
fn render_config(app: &App) -> String {
    let cursor = app.ui.navigation.status_pane.config_cursor;
    let mut s = String::new();
    for (i, row) in CONFIG_ROWS.iter().enumerate() {
        // The marker follows the shared list grammar every other pane uses,
        // so a reader who has moved a cursor elsewhere already knows what it
        // means here.
        let marker = if cursor == Some(i) { "❯ " } else { "  " };
        // The hint names the one key that acts. Only rows that act carry it:
        // a hint on a row that ignores the key is a promise the pane breaks.
        let hint = if row_changes(*row) {
            "   (Enter to change)"
        } else {
            ""
        };
        s.push_str(&field(
            &format!("{marker}{}", row_label(*row)),
            &format!("{}{hint}", row_value(app, *row)),
        ));
    }
    s.trim_end().to_string()
}

use super::usage::render_usage;
use crate::input::InputField;
use std::mem::take;

#[cfg(test)]
mod tests;
