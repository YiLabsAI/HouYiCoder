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
            Constraint::Length(1),
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
        let total = total_configured(&app.hook_entries);
        f.render_widget(
            Paragraph::new(format!("{total} hooks configured"))
                .style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
        f.render_widget(
            Paragraph::new("read-only; edit settings.json to configure")
                .style(Style::new().fg(Color::DarkGray)),
            chunks[2],
        );
        draw_event_list(f, chunks[3], app);
    } else {
        draw_event_detail(f, chunks[3], app);
    }
    let hint = if level == 0 {
        "Up/Down=move Enter=detail Esc=close"
    } else {
        "Esc=back"
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        chunks[4],
    );
}

fn framework_events(app: &App) -> Vec<&houyicoder_protocol::frontend::hooks::HookEntry> {
    app.hook_entries
        .iter()
        .filter(|h| h.source == "framework")
        .collect()
}

/// How many non-framework hooks match a given event name. Framework entries
/// are event definitions (not configured hooks); the rest are user/plugin
/// hooks. Used by the event list (the count shown inline + the sort key) and
/// the subtitle total.
fn configured_count(
    entries: &[houyicoder_protocol::frontend::hooks::HookEntry],
    event_name: &str,
) -> usize {
    entries
        .iter()
        .filter(|h| h.source != "framework" && h.events.iter().any(|ev| ev.as_str() == event_name))
        .count()
}

/// Total configured hooks across all events (non-framework entries).
fn total_configured(entries: &[houyicoder_protocol::frontend::hooks::HookEntry]) -> usize {
    entries.iter().filter(|h| h.source != "framework").count()
}

fn draw_event_list(f: &mut Frame, area: Rect, app: &App) {
    let events = framework_events(app);
    let counts: Vec<usize> = events
        .iter()
        .map(|e| configured_count(&app.hook_entries, &e.name))
        .collect();
    let mut order: Vec<usize> = (0..events.len()).collect();
    order.sort_by_key(|&i| counts[i] == 0);
    let sel = app.hooks_sel.get().min(events.len().saturating_sub(1));
    let items: Vec<ListItem> = order
        .iter()
        .map(|&i| {
            let e = events[i];
            let count = counts[i];
            let count_str = if count > 0 {
                format!(" ({count})")
            } else {
                String::new()
            };
            let mark = if count > 0 { " *" } else { "" };
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
    let mut events = framework_events(app);
    events.sort_by_key(|e| configured_count(&app.hook_entries, &e.name) == 0);
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
    if !event.description.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", event.description),
            Style::new().fg(Color::DarkGray),
        )));
    }
    if event.fired {
        lines.push(Line::from(Span::styled(
            "  live: this event has a fire point in the agent loop",
            Style::new().fg(Color::Green),
        )));
    }
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
    lines.push(Line::from(Span::styled(
        "  edit settings.json to configure",
        Style::new().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_protocol::frontend::hooks::HookEntry;

    fn fw(name: &str) -> HookEntry {
        HookEntry {
            name: name.into(),
            events: vec![name.into()],
            source: "framework".into(),
            fired: false,
            summary: "event def".into(),
            description: String::new(),
        }
    }

    fn hook(event: &str, source: &str) -> HookEntry {
        HookEntry {
            name: format!("hook-for-{event}"),
            events: vec![event.into()],
            source: source.into(),
            fired: false,
            summary: String::new(),
            description: String::new(),
        }
    }

    #[test]
    fn test_count_zero_no_hooks() {
        let entries = vec![fw("PreToolUse"), fw("PostToolUse")];
        assert_eq!(configured_count(&entries, "PreToolUse"), 0);
    }

    #[test]
    fn test_count_one_match() {
        let entries = vec![fw("PreToolUse"), hook("PreToolUse", "user")];
        assert_eq!(configured_count(&entries, "PreToolUse"), 1);
    }

    #[test]
    fn test_count_skips_framework() {
        let entries = vec![fw("PreToolUse"), hook("PreToolUse", "framework")];
        assert_eq!(configured_count(&entries, "PreToolUse"), 0);
    }

    #[test]
    fn test_total_non_framework() {
        let entries = vec![
            fw("PreToolUse"),
            hook("PreToolUse", "user"),
            hook("PostToolUse", "plugin"),
            fw("PostToolUse"),
        ];
        assert_eq!(total_configured(&entries), 2);
    }
}
