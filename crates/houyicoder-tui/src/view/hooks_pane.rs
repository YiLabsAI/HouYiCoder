//! /hooks pane content: a 2-level read-only browser for the hook surface.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::state::App;

pub(crate) const HOOKS_PANE_HEIGHT: u16 = 20;

pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);
    let level = app.hooks_level.get();
    let title = if level == 0 { "Hooks" } else { "Hook detail" };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            title,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );
    if level == 0 {
        draw_event_list(f, chunks[1], app);
    } else {
        draw_event_detail(f, chunks[1], app);
    }
    let hint = if level == 0 {
        "Up/Down=move Enter=detail Esc=close"
    } else {
        "Esc=back"
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn framework_events(app: &App) -> Vec<&houyicoder_protocol::frontend::hooks::HookEntry> {
    app.hook_entries
        .iter()
        .filter(|h| h.source == "framework")
        .collect()
}

fn draw_event_list(f: &mut Frame, area: Rect, app: &App) {
    let events = framework_events(app);
    let sel = app.hooks_sel.get().min(events.len().saturating_sub(1));
    let items: Vec<ListItem> = events
        .iter()
        .map(|e| {
            let count = app
                .hook_entries
                .iter()
                .filter(|h| h.source != "framework" && h.events.iter().any(|ev| ev == &e.name))
                .count();
            let count_str = if count > 0 {
                format!(" ({count})")
            } else {
                String::new()
            };
            let mark = if e.fired { " *" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<20}", e.name), Style::new().fg(Color::White)),
                Span::styled(
                    format!("{}{}", e.summary, count_str),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::styled(mark.to_string(), Style::new().fg(Color::Cyan)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(sel));
    f.render_stateful_widget(
        List::new(items)
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        area,
        &mut state,
    );
}

fn draw_event_detail(f: &mut Frame, area: Rect, app: &App) {
    let events = framework_events(app);
    let sel = app.hooks_sel.get().min(events.len().saturating_sub(1));
    if events.is_empty() {
        return;
    }
    let event = events[sel];
    let registered: Vec<_> = app
        .hook_entries
        .iter()
        .filter(|h| h.source != "framework" && h.events.iter().any(|ev| ev == &event.name))
        .collect();
    let mut lines = vec![Line::from(vec![
        Span::styled(
            event.name.clone(),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(event.summary.clone(), Style::new().fg(Color::DarkGray)),
    ])];
    if registered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no hooks configured for this event",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        for h in registered {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<18}", h.name), Style::new().fg(Color::White)),
                Span::styled(
                    format!("[{}] {}", h.source, h.events.join(", ")),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}
