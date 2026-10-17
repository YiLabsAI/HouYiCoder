//! The /agents pane: one list over the running children, the session's
//! returned delegations, and the types the model may delegate to, with a
//! detail drill-down on a type.
//!
//! Sections are always present when they have rows, so "what can I delegate
//! to" and "what has run" answer on one screen. The list body scrolls to keep
//! the cursor on screen, since a session with several children and several
//! returned delegations holds more rows than a half-height pane draws. The
//! detail holds a two-line header and scrolls only the fields, so the type and
//! its tier stay on screen while a long when-to-use or system prompt is read.

pub(crate) mod rows;
pub(crate) mod state;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use houyicoder_protocol::frontend::agents::{AgentEntry, AgentPrompt};
use unicode_width::UnicodeWidthStr;

use crate::command::render::format_tokens;
use crate::state::App;
use crate::view::agents_pane::rows::{AgentRow, pane_rows};
use crate::view::agents_pane::state::AgentDetail;
use crate::view::capability::titled_block;
use crate::view::line_wrap::{cursor_window_offset, max_scroll_offset, pad_width, truncate_width};
use crate::view::navigation::key_hint;

/// Rows the pane reserves when opened inline by the slash command.
pub(crate) const AGENTS_PANE_HEIGHT: u16 = 14;

/// The columns a when-to-use gets on its row before truncation: a description
/// past the budget clips rather than pushing the cost cell off the row.
const WHEN_TO_USE_BUDGET: usize = 40;

/// Render the agents pane inside a titled block. Used by the capability grid,
/// which hands over a bare rect; the inline slash-command pane draws its own
/// frame and calls draw_content with the inner rect instead.
pub(super) fn draw_agents(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "agents");
    let inner = block.inner(area);
    f.render_widget(block, area);
    draw_content(f, inner, app);
}

/// The pane's content: the type detail when one is open, otherwise the list.
pub(crate) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    if let Some(detail) = app.ui.agents_pane.detail() {
        draw_detail(f, area, app, detail);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let rows = pane_rows(app);
    f.render_widget(Paragraph::new(head_line(app, &rows)), chunks[0]);

    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(empty_line(app)).style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let selected = app
            .ui
            .agents_pane
            .cursor()
            .min(rows.len().saturating_sub(1));
        let (body, selected_line) = list_body(&rows, selected);
        let offset = cursor_window_offset(selected_line, body.len(), chunks[1].height);
        f.render_widget(Paragraph::new(body).scroll((offset, 0)), chunks[1]);
    }

    f.render_widget(
        Paragraph::new(key_hint(&[
            ("Up/Down", "select"),
            ("Enter", "open"),
            ("Esc", "close"),
        ]))
        .style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

/// The pane's head: what the list is, and how much of it there is. The counts
/// sit on the same line as the title, so the body below keeps every row the
/// pane can draw.
fn head_line(app: &App, rows: &[AgentRow<'_>]) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "Agents",
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )];
    let counts = count_line(app, rows);
    if !counts.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(counts, Style::new().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// The counts beside the title: how many types are delegatable, how many
/// children are running, how many delegations returned. A count whose source
/// has not reported yet is left out rather than printed as a zero the pane
/// never measured, and the returned count names the older ones the list holds
/// back.
fn count_line(app: &App, rows: &[AgentRow<'_>]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(entries) = app
        .sessions
        .current
        .capabilities
        .agent_types
        .entries
        .as_ref()
    {
        parts.push(format!("{} types", entries.len()));
    }
    let running = rows
        .iter()
        .filter(|row| matches!(row, AgentRow::Running(_)))
        .count();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    let completed = rows
        .iter()
        .filter(|row| matches!(row, AgentRow::Completed(_)))
        .count();
    if completed > 0 {
        // The list holds the newest delegations only, so the session's own
        // total is named beside them: a count that reported just the listed
        // rows would read as the session's total and under-report it.
        let returned = app.sessions.current.agents.rows.len();
        if returned > completed {
            parts.push(format!(
                "{completed} completed ({} older)",
                returned - completed
            ));
        } else {
            parts.push(format!("{completed} completed"));
        }
    }
    parts.join(" \u{00b7} ")
}

/// Placeholder for a list with no rows. A reply that has not landed and a
/// registry that registered nothing are different facts, so they do not share
/// one message.
fn empty_line(app: &App) -> &'static str {
    match app.sessions.current.capabilities.agent_types.entries {
        None => "(no agent types loaded)",
        Some(_) => "(no agent types registered)",
    }
}

/// The list body: a heading per section, then one row each, together with the
/// line the cursor stands on. A heading holds a line of its own, so the caller
/// needs the line number rather than the row index to scroll the selection
/// into view. Headings carry no action, so the cursor indexes rows only and no
/// key can land on a heading.
fn list_body(rows: &[AgentRow<'_>], cursor: usize) -> (Vec<Line<'static>>, usize) {
    let cursor = cursor.min(rows.len().saturating_sub(1));
    let type_width = widest_type(rows);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = 0;
    let mut previous: &str = "";
    for (index, row) in rows.iter().enumerate() {
        let section = row.section();
        if section != previous {
            lines.push(Line::from(Span::styled(
                format!("  {section}"),
                Style::new().fg(Color::Cyan),
            )));
            previous = section;
        }
        if index == cursor {
            selected_line = lines.len();
        }
        let prefix = if index == cursor { "▶ " } else { "  " };
        let mut spans = vec![Span::raw(prefix)];
        span_row(row, &mut spans, type_width);
        lines.push(Line::from(spans));
    }
    (lines, selected_line)
}

/// The width of the type cell, measured over the whole list: every row starts
/// its state, summary, or description at the same column, so the columns are a
/// decision about the pane rather than about the one row that holds the
/// longest name.
fn widest_type(rows: &[AgentRow<'_>]) -> usize {
    rows.iter()
        .map(|row| UnicodeWidthStr::width(row.subagent_type()))
        .max()
        .unwrap_or(0)
}

/// The body of one row, after its cursor prefix. The type cell is padded to
/// the list's own column width, and the description cell to its budget, so the
/// text behind either one begins at one column down the whole section.
fn span_row(row: &AgentRow<'_>, spans: &mut Vec<Span<'static>>, type_width: usize) {
    match row {
        AgentRow::Running(entry) => {
            spans.push(Span::styled(
                pad_width(&entry.subagent_type, type_width),
                Style::new().fg(Color::Yellow),
            ));
            spans.push(Span::styled(
                format!(
                    "  running · {} tok · turn {}",
                    format_tokens(entry.tokens),
                    entry.turn
                ),
                Style::new().fg(Color::DarkGray),
            ));
        }
        AgentRow::Completed(done) => {
            spans.push(Span::styled("✓ ", Style::new().fg(Color::Green)));
            spans.push(Span::styled(
                pad_width(&done.subagent_type, type_width),
                Style::new().fg(Color::Yellow),
            ));
            spans.push(Span::styled(
                format!(" · {}", done.summary),
                Style::new().fg(Color::DarkGray),
            ));
        }
        AgentRow::Available(entry) => {
            spans.push(Span::styled(
                format!("- {}", pad_width(&entry.subagent_type, type_width)),
                Style::new().fg(Color::Yellow),
            ));
            // The widest type takes no padding of its own, so the separator is
            // what keeps the description off the name.
            spans.push(Span::raw(format!(
                "  {}",
                pad_width(
                    &truncate_width(&entry.when_to_use, WHEN_TO_USE_BUDGET),
                    WHEN_TO_USE_BUDGET,
                )
            )));
            spans.push(Span::styled(
                format!(" · {}", model_cell(entry)),
                Style::new().fg(Color::DarkGray),
            ));
        }
    }
}

/// The cost cell of an available row: the model tier the type runs on, or
/// that it inherits the caller's. An absent tier is an inherited one, never a
/// tier named nothing.
fn model_cell(entry: &AgentEntry) -> String {
    match &entry.model {
        Some(model) => model.clone(),
        None => "inherits".to_string(),
    }
}

/// The type detail: a fixed two-line header (the type and its tier, then the
/// section it came from), the scrollable field body, and the key hints. Only
/// the body scrolls.
fn draw_detail(f: &mut Frame, inner: Rect, app: &App, detail: &AgentDetail) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let name = detail.subagent_type();
    let entry = app
        .sessions
        .current
        .capabilities
        .agent_types
        .entries
        .as_ref()
        .and_then(|entries| entries.iter().find(|e| e.subagent_type == name));
    f.render_widget(Paragraph::new(header_lines(entry, name)), chunks[0]);

    let body = field_lines(entry);
    let max_offset = max_scroll_offset(&body, chunks[1].width, chunks[1].height);
    app.ui.agents_pane.set_detail_max_offset(max_offset);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((app.ui.agents_pane.detail_offset().min(max_offset), 0)),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(key_hint(&[("Up/Down", "scroll"), ("Esc", "back")]))
            .style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

/// The identity row (type and tier) and the section row, matching what the
/// list shows so opening a row does not change what it is called.
fn header_lines(entry: Option<&AgentEntry>, name: &str) -> Vec<Line<'static>> {
    let mut head = vec![Span::styled(
        name.to_string(),
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )];
    if let Some(entry) = entry {
        head.push(Span::styled(
            format!("  ·  {}", model_cell(entry)),
            Style::new().fg(Color::DarkGray),
        ));
    }
    vec![
        Line::from(head),
        Line::from(Span::styled(
            "Available for delegation",
            Style::new().fg(Color::Cyan),
        )),
    ]
}

/// The definition's fields, one group each. A field the definition leaves
/// unset says it inherits rather than printing an empty value, and the fields
/// the pane does not show stay out of both the list and the detail.
fn field_lines(entry: Option<&AgentEntry>) -> Vec<Line<'static>> {
    let Some(entry) = entry else {
        return vec![Line::from(Span::styled(
            "(type no longer registered)",
            Style::new().fg(Color::DarkGray),
        ))];
    };
    // The header above names the type and its section, so the fields start a
    // row down: the first field otherwise reads as part of that header.
    let mut lines = vec![Line::from("")];
    lines.push(field_line("When to use: ", &entry.when_to_use));
    if let Some(effort) = entry.effort {
        lines.push(field_line("Effort: ", effort.label()));
    }
    let tools = match &entry.tools {
        Some(tools) if !tools.is_empty() => tools.join(", "),
        _ => "inherits the caller's tools".to_string(),
    };
    lines.push(field_line("Tools: ", &tools));
    if !entry.disallowed_tools.is_empty() {
        lines.push(field_line(
            "Disallowed: ",
            &entry.disallowed_tools.join(", "),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "System prompt",
        Style::new().fg(Color::Cyan),
    )));
    match &entry.system_prompt {
        AgentPrompt::Owned(body) => {
            lines.extend(body.lines().map(|line| Line::from(line.to_string())));
        }
        AgentPrompt::InheritParent => lines.push(Line::from(Span::styled(
            "inherits the caller's prompt",
            Style::new().fg(Color::DarkGray),
        ))),
    }
    lines
}

/// One labelled field line. The label carries the colour so the value reads
/// as the content and the label as the boundary.
fn field_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(label.to_string(), Style::new().fg(Color::Cyan)),
        Span::raw(value.to_string()),
    ])
}

#[cfg(test)]
mod tests;
