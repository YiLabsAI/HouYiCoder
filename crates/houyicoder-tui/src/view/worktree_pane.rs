//! The /worktrees pane body content. A peer of memory_pane draw_content and
//! capability draw_permission_content: the slash-command pane content closure
//! reused by the working-surface draw_command_pane template.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::state::App;

/// The /worktrees pane body: a header line with the count, one row per linked
/// worktree (current marker + path + short HEAD + branch), and a footer hint
/// naming the enter + remove actions. Pure render off app state. One
/// Paragraph of lines so the layout matches the /memory and /search panes
/// (predictable for render-cache tests); a long list wraps inside the area.
pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    let n = app.worktree_entries.len();
    let cursor = app.worktree_cursor.min(n.saturating_sub(1));
    lines.push(
        Line::from(format!("worktrees — {n} listed"))
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    );
    if app.worktree_entries.is_empty() {
        lines.push(
            Line::from("  (no worktrees; run from a git repo or add one with enter_worktree)")
                .style(Style::new().fg(Color::DarkGray)),
        );
    } else {
        for (i, e) in app.worktree_entries.iter().enumerate() {
            let prefix = if i == cursor { "❯ " } else { "  " };
            let cur_marker = if e.is_current { " * " } else { "   " };
            let path_style = if i == cursor {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            let mut spans: Vec<Span> = vec![
                Span::styled(prefix, Style::new().fg(Color::Cyan)),
                Span::styled(cur_marker, Style::new().fg(Color::Green)),
                Span::styled(format!("{} ", e.path), path_style),
                Span::styled(
                    format!("{} [{}]", e.head, e.branch),
                    Style::new().fg(Color::DarkGray),
                ),
            ];
            if e.is_current {
                spans.push(Span::styled("  (current)", Style::new().fg(Color::Green)));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from("  Enter enter · d remove (asks) · Esc close")
            .style(Style::new().fg(Color::DarkGray)),
    );
    f.render_widget(Paragraph::new(lines), area);
}
