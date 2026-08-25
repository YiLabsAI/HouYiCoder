//! The /worktrees pane body content. Uses shared ListPaneState helpers
//! (truncate_path, search_hint_line) so the pattern is reusable across
//! panes. Mirrors the /hooks pane layout (header + scrollable List + fixed
//! footer) so the cursor stays visible when the list is longer than the
//! pane and the footer hint stays pinned.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::composition::WorktreeEntry;
use crate::list_pane_state::{filter_by_query, search_hint_line, truncate_path};
use crate::state::App;

/// Max width for the path column. Long paths are left-truncated (tail kept)
/// so the identifying segment stays visible. Fixed width so the column
/// boundary is stable for mouse selection.
const PATH_WIDTH: usize = 32;

pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let n = app.worktree_entries.len();
    // Level 1 detail (Enter on the list opens this).
    if app.worktree_level.get() == 1 && n > 0 {
        let cur = app.worktree_list.cursor.min(n.saturating_sub(1));
        if let Some(e) = app.worktree_entries.get(cur) {
            draw_detail(f, area, e);
            return;
        }
    }
    let searching = app.worktree_list.searching();
    let search_h: u16 = if searching { 1 } else { 0 };
    // Header + optional search hint + scrollable list + fixed footer. The
    // list is a stateful List widget (not a Paragraph dump) so the cursor
    // stays visible when the list is longer than the pane and the footer
    // hint stays pinned at the bottom — same shape as the /hooks pane.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(search_h),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!("worktrees — {n} listed"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );
    if searching {
        f.render_widget(
            Paragraph::new(search_hint_line(&app.worktree_list.query))
                .style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    }
    let filtered_idx: Vec<usize> = filter_by_query(
        &app.worktree_entries,
        &app.worktree_list.query,
        |e: &WorktreeEntry| vec![&e.path, &e.branch],
    );
    let cur = app.worktree_list.cursor.min(n.saturating_sub(1));
    if filtered_idx.is_empty() {
        let msg = if app.worktree_entries.is_empty() {
            "  (no worktrees; run from a git repo or add one with enter_worktree)"
        } else {
            "  (no worktrees match the search)"
        };
        f.render_widget(
            Paragraph::new(msg).style(Style::new().fg(Color::DarkGray)),
            chunks[2],
        );
    } else {
        let cursor_pos = filtered_idx.iter().position(|&i| i == cur);
        let items = worktree_items(app, &filtered_idx, cur);
        let mut state = ListState::default();
        state.select(cursor_pos);
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("❯ "),
            chunks[2],
            &mut state,
        );
    }
    f.render_widget(
        Paragraph::new("  Up/Down=move Enter=detail e=enter Esc=close")
            .style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );
}

/// Level 1 detail view: full path (not truncated) + branch + HEAD + current
/// marker. dirty/time/type are deferred (parse_worktrees does not provide
/// them; the design's display-only round). 'e' in this view opens the
/// worktree; Esc returns to the list.
fn draw_detail(f: &mut Frame, area: Rect, e: &WorktreeEntry) {
    let slug = std::path::Path::new(&e.path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| e.path.clone());
    let mut lines = vec![
        Line::from(vec![Span::styled(
            format!("Worktree: {slug}"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]),
        Line::from(format!("Path:     {}", e.path)),
        Line::from(format!("Branch:   {}", e.branch)),
        Line::from(format!("HEAD:     {}", e.head)),
    ];
    if e.is_current {
        lines.push(Line::from("Status:   current worktree"));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("  e=enter Esc=back to list").style(Style::new().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(lines), area);
}

/// Build the list rows (one per filtered worktree). The cursor is an index
/// into worktree_entries (not the filtered list); when search is active the
/// cursor's item may be filtered out, in which case no row is selected.
fn worktree_items(app: &App, filtered_idx: &[usize], cur: usize) -> Vec<ListItem<'static>> {
    filtered_idx
        .iter()
        .map(|&idx| {
            let e = &app.worktree_entries[idx];
            let is_cursor = idx == cur;
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
            ListItem::new(Line::from(spans))
        })
        .collect()
}
