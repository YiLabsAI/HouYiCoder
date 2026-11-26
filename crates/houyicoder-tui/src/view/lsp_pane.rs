//! /lsp pane content: the configured language servers as a list, with a
//! detail drill-down per server. The list shows state, scope, and the
//! suffixes each server owns; the detail pins the server's identity and
//! scrolls its command, the open-document and diagnostic counts, and the
//! stderr tail.

pub(crate) mod state;

use houyicoder_platform::desktop::glyphs::detect_glyph_set;
use houyicoder_protocol::lsp::LspConfigScope;
use houyicoder_protocol::lsp::LspServerState;
use houyicoder_protocol::lsp::LspServerStatusView;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::lsp_pane_state::LspPaneState;
use crate::state::App;
use crate::view::line_wrap::cursor_window_offset;
use crate::view::line_wrap::max_scroll_offset;
use crate::view::navigation::key_hint;

pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    // The detail level is only meaningful with a selected row; a stale level
    // over an empty list falls back to the list rather than drawing nothing.
    if app.ui.lsp_pane.level == 1 && app.ui.lsp_pane.selected().is_some() {
        draw_detail(f, inner, app);
    } else {
        draw_list(f, inner, app);
    }
}

/// The list: a one-row header, the server rows, and the key hints.
fn draw_list(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    let pane = &app.ui.lsp_pane;
    let running = pane
        .rows
        .iter()
        .filter(|row| {
            matches!(
                row.state,
                LspServerState::Running | LspServerState::Indexing { .. }
            )
        })
        .count();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "LSP",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} servers · {running} running", pane.rows.len()),
                Style::new().fg(Color::DarkGray),
            ),
        ])),
        chunks[0],
    );

    let mut body: Vec<Line<'static>> = Vec::new();
    if pane.rows.is_empty() {
        body.push(
            Line::from("no language servers configured").style(Style::new().fg(Color::DarkGray)),
        );
    } else {
        for (idx, row) in pane.rows.iter().enumerate() {
            let prefix = if idx == pane.cursor {
                detect_glyph_set().picker_cursor().to_string()
            } else {
                "  ".to_string()
            };
            let (state_label, color) = state_label(row);
            body.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(format!("{} ", row.name), Style::new().fg(Color::Yellow)),
                Span::styled(state_label, Style::new().fg(color)),
                Span::styled(
                    format!("  {} · {}", scope_label(row.scope), suffixes(row)),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    let offset = cursor_window_offset(pane.cursor, body.len(), chunks[1].height);
    f.render_widget(Paragraph::new(body).scroll((offset, 0)), chunks[1]);

    f.render_widget(
        Paragraph::new(list_hint(pane)).style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

/// The list's key hints, trimmed to what the selected row accepts: an empty
/// list has nothing to open, a locked row offers no toggle, and a disabled
/// one offers no restart.
fn list_hint(pane: &LspPaneState) -> Line<'static> {
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if !pane.rows.is_empty() {
        pairs.push(("Up/Down", "select"));
        pairs.push(("Enter", "open"));
    }
    if pane.allows_restart() {
        pairs.push(("r", "restart"));
    }
    if pane.allows_toggle() {
        pairs.push(("t", "toggle"));
    }
    pairs.push(("Esc", "close"));
    key_hint(&pairs)
}

/// The detail: a pinned two-row header, the scrollable body, and the key
/// hints. Only the body region scrolls, so the server's identity stays
/// visible however long its stderr tail is.
fn draw_detail(f: &mut Frame, inner: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let pane = &app.ui.lsp_pane;
    let Some(view) = pane.selected() else {
        return;
    };

    let (state_label, color) = state_label(view);
    let header = vec![
        Span::styled(
            view.name.clone(),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {state_label}"), Style::new().fg(color)),
    ];
    let sub = vec![Span::styled(
        format!("{} scope · {}", scope_label(view.scope), suffixes(view)),
        Style::new().fg(Color::DarkGray),
    )];
    f.render_widget(
        Paragraph::new(vec![Line::from(header), Line::from(sub)]),
        rows[0],
    );

    let body = detail_body(view);
    let max_offset = max_scroll_offset(&body, rows[1].width, rows[1].height);
    pane.set_detail_max_offset(max_offset);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((pane.detail_offset().min(max_offset), 0)),
        rows[1],
    );

    f.render_widget(
        Paragraph::new(detail_hint(pane)).style(Style::new().fg(Color::DarkGray)),
        rows[2],
    );
}

/// The detail's key hints, trimmed on the same rule as the list's.
fn detail_hint(pane: &LspPaneState) -> Line<'static> {
    let mut pairs: Vec<(&str, &str)> = vec![("Up/Down", "scroll")];
    if pane.allows_restart() {
        pairs.push(("r", "restart"));
    }
    if pane.allows_toggle() {
        pairs.push(("t", "toggle"));
    }
    pairs.push(("Esc", "back"));
    key_hint(&pairs)
}

/// The detail body lines: the command as configured, the open-document and
/// diagnostic counts, and the stderr tail.
fn detail_body(view: &LspServerStatusView) -> Vec<Line<'static>> {
    let gray = Style::new().fg(Color::DarkGray);
    let mut body: Vec<Line<'static>> = Vec::new();
    body.push(Line::from(vec![
        Span::styled("command  ", gray),
        Span::raw(view.command_line.clone()),
    ]));
    body.push(Line::from(vec![
        Span::styled("documents  ", gray),
        Span::raw(view.open_documents.to_string()),
    ]));
    body.push(Line::from(vec![
        Span::styled("diagnostics  ", gray),
        Span::raw(view.diagnostic_count.to_string()),
    ]));
    if !view.recent_stderr.is_empty() {
        body.push(Line::from("stderr (recent)").style(gray));
        body.extend(
            view.recent_stderr
                .iter()
                .map(|l| Line::from(format!("  {l}")).style(gray)),
        );
    }
    body
}

/// The suffixes one server owns, as a comma list.
fn suffixes(view: &LspServerStatusView) -> String {
    view.extensions
        .iter()
        .map(|ext| format!(".{ext}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The state's label and colour as both levels render it. A disabled row
/// the trust gate holds reads as locked, so the reason is not mistaken for
/// a choice the user made.
fn state_label(view: &LspServerStatusView) -> (String, Color) {
    match &view.state {
        LspServerState::Idle => ("idle".into(), Color::DarkGray),
        LspServerState::Starting => ("starting".into(), Color::Yellow),
        LspServerState::Indexing { message } => match message {
            Some(message) if !message.is_empty() => {
                (format!("indexing · {message}"), Color::Yellow)
            }
            _ => ("indexing".into(), Color::Yellow),
        },
        LspServerState::Running => ("running".into(), Color::Green),
        LspServerState::Failed { error } => (format!("failed · {error}"), Color::Red),
        LspServerState::Disabled { reason } => {
            if view.locked {
                ("locked · untrusted workspace".into(), Color::Red)
            } else {
                (format!("disabled · {reason}"), Color::DarkGray)
            }
        }
    }
}

/// The scope's label, the same vocabulary the status view uses.
fn scope_label(scope: LspConfigScope) -> &'static str {
    match scope {
        LspConfigScope::User => "user",
        LspConfigScope::Project => "project",
        LspConfigScope::Plugin => "plugin",
        LspConfigScope::PluginCache => "plugin cache",
    }
}

#[cfg(test)]
mod tests;
