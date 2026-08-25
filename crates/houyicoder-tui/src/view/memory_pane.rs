//! The /memory pane body content. A peer of search.rs draw_content and
//! capability draw_permission_content: the slash-command pane content
//! closures reused by the working-surface draw_command_pane template.
//! Mirrors the /hooks + /worktrees shape: a fixed header block, a scrollable
//! stateful List for the memory rows (cursor stays visible when the list is
//! longer than the pane), and fixed toggle + footer rows at the bottom.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::state::App;
use crate::state::MemoryEntry;
use crate::state::enums::MemoryScopeTab;

/// The /memory pane body: a fixed header (scope tabs + stored count +
/// optional search hint + scope distribution), a scrollable list of memory
/// rows (the cursor stays visible when the list is longer than the pane),
/// and fixed toggle + footer rows. The list is a stateful List widget, not a
/// Paragraph dump, so the cursor never scrolls out of view and the
/// toggles/footer stay pinned.
pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let tab = app.memory_scope_tab;
    let filtered =
        crate::command::render::filtered_memory(&app.memory_entries, tab, &app.memory_list.query);
    let n = filtered.len();
    let cursor = app.memory_list.cursor.min(n.saturating_sub(1));
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
        let items = memory_items(&filtered, cursor);
        let mut state = ListState::default();
        state.select(Some(cursor));
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("❯ "),
            chunks[1],
            &mut state,
        );
    }
    let am = if app.memory_toggles.auto_memory {
        "on"
    } else {
        "off"
    };
    let ad = if app.memory_toggles.auto_dream {
        "on"
    } else {
        "off"
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Auto-memory: ", Style::new().fg(Color::DarkGray)),
                Span::styled(am, Style::new().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  Auto-dream: ", Style::new().fg(Color::DarkGray)),
                Span::styled(ad, Style::new().fg(Color::White)),
            ]),
            Line::from(
                "  /memory <key> show · /memory toggle auto|dream · Left/Right scope · Esc close",
            )
            .style(Style::new().fg(Color::DarkGray)),
        ]),
        chunks[2],
    );
}

/// The fixed header block: scope tabs, optional search hint, stored count,
/// optional scope distribution. Built separately so draw_content stays
/// under the too-many-lines gate.
fn memory_header(app: &App, tab: MemoryScopeTab, n: usize) -> Vec<Line<'static>> {
    let tabs = [
        (MemoryScopeTab::All, "All"),
        (MemoryScopeTab::User, "User"),
        (MemoryScopeTab::Project, "Project"),
        (MemoryScopeTab::Auto, "Auto"),
    ];
    let mut tab_spans: Vec<Span> = vec![Span::raw("  ")];
    for (i, (variant, label)) in tabs.iter().enumerate() {
        if i > 0 {
            tab_spans.push(Span::raw("  "));
        }
        if *variant == tab {
            tab_spans.push(Span::styled(
                format!("[{label}]"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        } else {
            tab_spans.push(Span::styled(*label, Style::new().fg(Color::DarkGray)));
        }
    }
    let mut lines: Vec<Line> = vec![Line::from(tab_spans)];
    if app.memory_list.searching() {
        lines.push(
            Line::from(format!(
                "  {}",
                crate::list_pane_state::search_hint_line(&app.memory_list.query)
            ))
            .style(Style::new().fg(Color::DarkGray)),
        );
    }
    lines.push(
        Line::from(format!("memory — {n} stored"))
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    );
    if !app.memory_entries.is_empty() {
        lines.push(
            Line::from(scope_distribution(&app.memory_entries))
                .style(Style::new().fg(Color::DarkGray)),
        );
    }
    lines
}

/// The scrollable memory rows as List items. The cursor row is selected via
/// ListState (built by the caller) so ratatui scrolls it into view.
fn memory_items(filtered: &[&MemoryEntry], cursor: usize) -> Vec<ListItem<'static>> {
    filtered
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let is_cursor = i == cursor;
            let tag = format!("{}·{}", m.scope, m.source);
            let key_style = if is_cursor {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            let mut spans: Vec<Span> = vec![
                Span::styled(format!("[{tag}] "), Style::new().fg(Color::DarkGray)),
                Span::styled(m.topic.clone(), key_style),
            ];
            if !m.summary.is_empty() {
                spans.push(Span::styled(
                    format!(" — {}", m.summary),
                    Style::new().fg(Color::White),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect()
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
