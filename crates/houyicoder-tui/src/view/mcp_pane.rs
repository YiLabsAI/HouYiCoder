//! /mcp pane content: the configured external tool servers as a list, with
//! a detail drill-down per server. The list shows state, scope, and counts;
//! the detail pins the server's identity and scrolls its transport, masked
//! headers, tools, instructions preview, and stderr.

pub(crate) mod state;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use houyicoder_protocol::mcp::{McpConfigScope, McpConnectionState, McpServerStatusView};

use crate::mcp_pane_state::McpPaneState;
use crate::state::App;
use crate::view::line_wrap::cursor_window_offset;
use crate::view::line_wrap::max_scroll_offset;
use crate::view::navigation::key_hint;
use houyicoder_platform::desktop::glyphs::detect_glyph_set;

/// The masked value a rendered header carries. The host sends header names
/// only, so a configured secret never reaches a view that renders it.
const MASKED_HEADER_VALUE: &str = "***";

pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    if app.ui.mcp_pane.level == 1 {
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

    let pane = &app.ui.mcp_pane;
    let connected = pane
        .rows
        .iter()
        .filter(|row| matches!(row.state, McpConnectionState::Connected))
        .count();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "MCP",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} servers · {connected} connected", pane.rows.len()),
                Style::new().fg(Color::DarkGray),
            ),
        ])),
        chunks[0],
    );

    let mut body: Vec<Line<'static>> = Vec::new();
    if pane.rows.is_empty() {
        body.push(Line::from("no servers configured").style(Style::new().fg(Color::DarkGray)));
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
                    format!("  {} · {}", scope_label(row.scope), row.tool_count),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::styled(
                    if row.tool_count == 1 {
                        " tool"
                    } else {
                        " tools"
                    },
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

/// The list's key hints, trimmed to what the selected row accepts: a locked
/// row offers neither action key, and a disabled one offers no reconnect.
fn list_hint(pane: &McpPaneState) -> Line<'static> {
    let mut pairs: Vec<(&str, &str)> = vec![("Up/Down", "select"), ("Enter", "open")];
    if pane.allows_reconnect() {
        pairs.push(("r", "reconnect"));
    }
    if pane.allows_toggle() {
        pairs.push(("t", "toggle"));
    }
    pairs.push(("Esc", "close"));
    key_hint(&pairs)
}

/// The detail: a pinned two-row header, the scrollable body, and the key
/// hints. Only the body region scrolls, so the server's identity stays
/// visible however long its diagnostics are.
fn draw_detail(f: &mut Frame, inner: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let pane = &app.ui.mcp_pane;
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
    let mut sub = vec![Span::styled(
        format!("{} scope", scope_label(view.scope)),
        Style::new().fg(Color::DarkGray),
    )];
    if let Some(version) = &view.server_version {
        sub.push(Span::styled(
            format!(" · server {version}"),
            Style::new().fg(Color::DarkGray),
        ));
    }
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
fn detail_hint(pane: &McpPaneState) -> Line<'static> {
    let mut pairs: Vec<(&str, &str)> = vec![("Up/Down", "scroll")];
    if pane.allows_reconnect() {
        pairs.push(("r", "reconnect"));
    }
    if pane.allows_toggle() {
        pairs.push(("t", "toggle"));
    }
    pairs.push(("Esc", "back"));
    key_hint(&pairs)
}

/// The detail body lines: transport, masked headers, tools with their
/// safety labels, the instructions preview, and the stderr tail.
fn detail_body(view: &McpServerStatusView) -> Vec<Line<'static>> {
    let detail = &view.detail;
    let gray = Style::new().fg(Color::DarkGray);
    let mut body: Vec<Line<'static>> = Vec::new();
    body.push(Line::from(vec![
        Span::styled("transport  ", gray),
        Span::raw(detail.endpoint.clone()),
    ]));
    if !detail.header_names.is_empty() {
        body.push(Line::from("headers").style(gray));
        for name in &detail.header_names {
            body.push(Line::from(format!("  {name}: {MASKED_HEADER_VALUE}")));
        }
    }
    if !detail.tools.is_empty() {
        body.push(Line::from(format!("tools ({})", detail.tools.len())).style(gray));
        for tool in &detail.tools {
            let label = if tool.read_only {
                "[read-only]"
            } else {
                "[approval-required]"
            };
            let mut line = format!("  {} {label}", tool.name);
            if !tool.description.is_empty() {
                line.push_str(&format!(" · {}", tool.description));
            }
            body.push(Line::from(line));
        }
    }
    if let Some(preview) = &detail.instructions_preview {
        body.push(Line::from("instructions").style(gray));
        body.extend(preview.lines().map(|l| Line::from(format!("  {l}"))));
    }
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

/// The state's label and colour as both levels render it.
fn state_label(view: &McpServerStatusView) -> (String, Color) {
    match &view.state {
        McpConnectionState::Connected => ("connected".into(), Color::Green),
        McpConnectionState::Pending => ("starting".into(), Color::Yellow),
        McpConnectionState::Reconnecting { attempt } => {
            (format!("reconnecting (attempt {attempt})"), Color::Yellow)
        }
        McpConnectionState::NeedsAuth { reason } => {
            (format!("needs auth · {reason}"), Color::Magenta)
        }
        McpConnectionState::Failed { error } => (format!("failed · {error}"), Color::Red),
        McpConnectionState::Disabled { .. } => {
            if view.detail.untrusted_lock {
                ("locked · untrusted workspace".into(), Color::Red)
            } else {
                ("disabled".into(), Color::DarkGray)
            }
        }
    }
}

/// The scope's label, the same vocabulary the status view uses.
fn scope_label(scope: McpConfigScope) -> &'static str {
    match scope {
        McpConfigScope::User => "user",
        McpConfigScope::Project => "project",
        McpConfigScope::Env => "env",
    }
}

#[cfg(test)]
mod tests;
