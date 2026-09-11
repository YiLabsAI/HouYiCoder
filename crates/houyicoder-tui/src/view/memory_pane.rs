//! Memory browser presentation.
//!
//! The list preserves the provider's newest-first order and combines scope
//! filtering with text search. Each row keeps identity, provenance, recency,
//! and its bounded summary aligned with the same cursor used by view and
//! forget actions. Opening a row replaces the list with its full detail; Esc
//! returns to the list. Tabs, counts, memory switches, and action hints remain
//! fixed while only the entry region scrolls.

use houyicoder_protocol::frontend::memory::MemoryDetail;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use crate::memory_state::{MemoryDetailState, MemoryPaneState};
use crate::state::enums::MemoryScopeTab;
use crate::state::{App, MemoryEntry};
use crate::view::line_wrap::{truncate_width, wrap_styled_line};
use crate::view::navigation::{key_hint, tab_header};

/// Render the memory list or the selected memory detail.
pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    match app.memory.detail() {
        Some(MemoryDetailState::Loading { key, .. }) => {
            draw_loading(f, area, key);
            return;
        }
        Some(MemoryDetailState::Open { entry, offset, .. }) => {
            draw_detail(f, area, &app.memory, entry, offset.get());
            return;
        }
        None => {}
    }
    let tab = app.memory.scope();
    let filtered = app.memory.filtered();
    let n = filtered.len();
    let cursor = app.memory.cursor().min(n.saturating_sub(1));
    let header_lines = memory_header(app, tab, n);
    let header_h = header_lines.len() as u16;
    // Header (fixed) + scrollable list + bottom (toggles + footer, fixed).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);
    f.render_widget(Paragraph::new(header_lines), chunks[0]);
    if filtered.is_empty() {
        f.render_widget(
            Paragraph::new("  (no memories yet)").style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let items = memory_items(&filtered, cursor, chunks[1].width.saturating_sub(2));
        let mut state = ListState::default();
        state.select(Some(cursor));
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::default().bg(Color::Indexed(238)))
                .highlight_symbol("❯ "),
            chunks[1],
            &mut state,
        );
    }
    let auto_action = if app.memory.toggles().auto_memory {
        "disable auto-memory"
    } else {
        "enable auto-memory"
    };
    let dream_action = if app.memory.toggles().auto_dream {
        "disable auto-dream"
    } else {
        "enable auto-dream"
    };
    f.render_widget(
        Paragraph::new(vec![
            key_hint(&[("a", auto_action), ("c", dream_action)]),
            key_hint(&[("Up/Down", "select"), ("Enter", "open")]),
            key_hint(&[
                ("d", "forget"),
                ("Tab/Left/Right", "switch scope"),
                ("Esc", "close"),
            ]),
        ]),
        chunks[2],
    );
}

fn draw_loading(f: &mut Frame, area: Rect, key: &str) {
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  memory  ", Style::new().fg(Color::Cyan)),
                Span::styled(
                    key.to_string(),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from("  loading detail...").style(Style::new().fg(Color::DarkGray)),
            key_hint(&[("Esc", "back")]),
        ]),
        area,
    );
}

fn draw_detail(
    f: &mut Frame,
    area: Rect,
    memory: &MemoryPaneState,
    detail: &MemoryDetail,
    offset: u16,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  memory  ", Style::new().fg(Color::Cyan)),
                Span::styled(
                    detail.key.clone(),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(format!("  source: {}", detail.source))
                .style(Style::new().fg(Color::DarkGray)),
        ]),
        rows[0],
    );
    let mut body = vec![
        Line::from(format!("updated: {}", age_label(detail.mtime_secs)))
            .style(Style::new().fg(Color::DarkGray)),
        Line::from(""),
    ];
    let description = detail.description.trim();
    if !description.is_empty() && !detail.content.trim_start().starts_with(description) {
        body.push(Line::from(description.to_string()).style(Style::new().fg(Color::DarkGray)));
        body.push(Line::from(""));
    }
    body.extend(
        detail
            .content
            .lines()
            .map(|line| Line::from(line.to_string())),
    );
    let max_offset = detail_max_offset(&body, rows[1].width, rows[1].height);
    memory.set_detail_max_offset(max_offset);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((offset.min(max_offset), 0)),
        rows[1],
    );
    f.render_widget(
        Paragraph::new(key_hint(&[("Up/Down", "scroll"), ("Esc", "back")])),
        rows[2],
    );
}

/// Return the largest scroll offset after display-width wrapping.
pub(crate) fn detail_max_offset(body: &[Line<'static>], width: u16, height: u16) -> u16 {
    let rendered_rows: usize = body
        .iter()
        .cloned()
        .map(|line| wrap_styled_line(line, width as usize).len())
        .sum();
    u16::try_from(rendered_rows.saturating_sub(height as usize)).unwrap_or(u16::MAX)
}

fn memory_header(app: &App, tab: MemoryScopeTab, n: usize) -> Vec<Line<'static>> {
    let tabs = [
        (MemoryScopeTab::All, "All"),
        (MemoryScopeTab::User, "User"),
        (MemoryScopeTab::Project, "Project"),
        (MemoryScopeTab::Auto, "Auto"),
    ];
    let mut lines = vec![tab_header(tab, &tabs)];
    if app.memory.searching() {
        lines.push(
            Line::from(format!(
                "  {}",
                crate::list_pane_state::search_hint_line(app.memory.search_query())
            ))
            .style(Style::new().fg(Color::DarkGray)),
        );
    }
    lines.push(
        Line::from(format!("  memory — {n} stored · newest first"))
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    );
    if !app.memory.entries().is_empty() {
        lines.push(
            Line::from(scope_distribution(app.memory.entries()))
                .style(Style::new().fg(Color::DarkGray)),
        );
    }
    lines
}

/// Build two-line list items: identity and recency first, description second.
fn memory_items(filtered: &[&MemoryEntry], cursor: usize, width: u16) -> Vec<ListItem<'static>> {
    filtered
        .iter()
        .enumerate()
        .map(|(i, memory)| {
            let key_style = if i == cursor {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            let scope = format!("[{}] ", memory.scope);
            let meta = format!("  {} · {}", memory.source, age_label(memory.mtime_secs));
            let key_width = (width as usize)
                .saturating_sub(unicode_width::UnicodeWidthStr::width(scope.as_str()))
                .saturating_sub(unicode_width::UnicodeWidthStr::width(meta.as_str()));
            let key = truncate_width(&memory.topic, key_width);
            let summary_width = (width as usize).saturating_sub(2);
            let summary = truncate_width(&memory.summary, summary_width);
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(scope, Style::new().fg(Color::DarkGray)),
                    Span::styled(key, key_style),
                    Span::styled(meta, Style::new().fg(Color::DarkGray)),
                ]),
                Line::from(Span::styled(
                    format!("  {summary}"),
                    Style::new().fg(Color::DarkGray),
                )),
            ])
        })
        .collect()
}

fn age_label(mtime_secs: u64) -> String {
    if mtime_secs == 0 {
        return "unknown".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(mtime_secs);
    let age = now.saturating_sub(mtime_secs);
    match age {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m ago", age / 60),
        3_600..=86_399 => format!("{}h ago", age / 3_600),
        _ => format!("{}d ago", age / 86_400),
    }
}

/// One-line breakdown of how many memories live in each storage scope, across
/// the full entry list (not the filtered view). Shown dim under the header so
/// the user sees the distribution at a glance even when narrowed to one scope.
fn scope_distribution(entries: &[MemoryEntry]) -> String {
    let (mut u, mut p, mut a) = (0usize, 0usize, 0usize);
    for m in entries {
        match m.scope.as_str() {
            "user" => u += 1,
            "project" => p += 1,
            "auto" => a += 1,
            _ => {}
        }
    }
    format!("  {u} user / {p} project / {a} auto")
}
