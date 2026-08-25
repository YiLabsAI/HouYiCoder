//! The /worktrees pane body content. Uses shared ListPaneState helpers
//! (truncate_path, search_hint_line) so the pattern is reusable across
//! panes — the first pane to adopt the template.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::list_pane_state::{filter_by_query, search_hint_line, truncate_path};
use crate::state::App;

/// Max width for the path column. Long paths are left-truncated (tail kept)
/// so the identifying segment stays visible. Fixed width so the column
/// boundary is stable for mouse selection.
const PATH_WIDTH: usize = 32;

pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    let n = app.worktree_entries.len();
    lines.push(
        Line::from(format!("worktrees — {n} listed"))
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    );
    if app.worktree_list.searching() {
        lines.push(
            Line::from(search_hint_line(&app.worktree_list.query))
                .style(Style::new().fg(Color::DarkGray)),
        );
    }
    let filtered_idx: Vec<usize> = filter_by_query(
        &app.worktree_entries,
        &app.worktree_list.query,
        |e: &crate::composition::WorktreeEntry| vec![&e.path, &e.branch],
    );
    if filtered_idx.is_empty() {
        if app.worktree_entries.is_empty() {
            lines.push(
                Line::from("  (no worktrees; run from a git repo or add one with enter_worktree)")
                    .style(Style::new().fg(Color::DarkGray)),
            );
        } else {
            lines.push(
                Line::from("  (no worktrees match the search)")
                    .style(Style::new().fg(Color::DarkGray)),
            );
        }
    } else {
        // The cursor is an index into worktree_entries (not the filtered
        // list). When search is active, the cursor's item may be filtered
        // out; in that case no marker shows. Actions always use the
        // original cursor, so there is no disconnect between what the user
        // sees and what Enter/d does.
        let cur = app.worktree_list.cursor.min(n.saturating_sub(1));
        for &idx in &filtered_idx {
            let e = &app.worktree_entries[idx];
            let is_cursor = idx == cur;
            let prefix = if is_cursor { "❯ " } else { "  " };
            let cur_marker = if e.is_current { " * " } else { "   " };
            let path_style = if is_cursor {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            let path = truncate_path(&e.path, PATH_WIDTH);
            let branch = if e.branch.chars().count() > 16 {
                let b: String = e.branch.chars().take(15).collect();
                format!("{b}\u{2026}")
            } else {
                e.branch.clone()
            };
            let mut spans: Vec<Span> = vec![
                Span::styled(prefix, Style::new().fg(Color::Cyan)),
                Span::styled(cur_marker, Style::new().fg(Color::Green)),
                Span::styled(format!("{:<width$}", path, width = PATH_WIDTH), path_style),
                Span::styled(
                    format!(" {} [{}]", e.head, branch),
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
        Line::from("  Up/Down=move Enter=enter d=remove(asks) Esc=close")
            .style(Style::new().fg(Color::DarkGray)),
    );
    f.render_widget(Paragraph::new(lines), area);
}
