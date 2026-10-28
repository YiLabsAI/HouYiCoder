//! The /worktrees pane at its three levels: the list, one worktree's detail,
//! and the removal confirmation. The list is a stateful List widget so the
//! cursor stays visible when the pane is shorter than the list; the detail and
//! the confirmation are fixed blocks whose last row names the keys that act
//! on them.

use houyicoder_protocol::frontend::WorktreePreviewView;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::composition::WorktreeEntry;
use crate::list_pane_state::{filter_by_query, search_hint_line, truncate_path};
use crate::state::App;
use crate::ui::{RemoveFocus, WorktreeLevel};
use crate::view::navigation::{block_with_keys, key_hint, key_row_height, tab_header};
use crate::view::relative_time::now_epoch_secs;
use crate::view::relative_time::relative_time;

/// Max width for the path column. Long paths are left-truncated (tail kept)
/// so the identifying segment stays visible. Fixed width so the column
/// boundary is stable for mouse selection.
const PATH_WIDTH: usize = 32;

pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    match app.ui.worktree_pane.level.get() {
        WorktreeLevel::Detail => draw_detail(f, area, app),
        WorktreeLevel::Confirm => draw_confirm(f, area, app),
        WorktreeLevel::List => draw_list(f, area, app),
    }
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let entries = &app.sessions.current.capabilities.worktree.entries;
    let n = entries.len();
    let searching = app.ui.worktree_pane.search.get() || app.ui.worktree_pane.list.searching();
    let search_h: u16 = if searching { 1 } else { 0 };
    let footer = key_hint(&[
        ("Up/Down", "select"),
        ("Enter", "open"),
        ("e", "enter"),
        ("d", "remove"),
        ("/", "search"),
        ("Esc", "close"),
    ]);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(search_h),
            Constraint::Min(0),
            Constraint::Length(key_row_height(&footer, area)),
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
            Paragraph::new(search_hint_line(&app.ui.worktree_pane.list.query))
                .style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    }
    let filtered_idx: Vec<usize> = filter_by_query(
        entries,
        &app.ui.worktree_pane.list.query,
        |e: &WorktreeEntry| vec![&e.path, &e.branch],
    );
    let cur = app.ui.worktree_pane.list.cursor.min(n.saturating_sub(1));
    if filtered_idx.is_empty() {
        let msg = if entries.is_empty() {
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
        let items = worktree_items(entries, &filtered_idx, cur);
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
        Paragraph::new(footer)
            .wrap(Wrap { trim: false })
            .style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );
}

/// The detail of one worktree: the fields the listing holds, then what the
/// host measured about a removal. A field the host has not answered yet shows
/// the listing's own value instead of a zero.
fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let entries = &app.sessions.current.capabilities.worktree.entries;
    let cur = app
        .ui
        .worktree_pane
        .list
        .cursor
        .min(entries.len().saturating_sub(1));
    let Some(entry) = entries.get(cur) else {
        f.render_widget(
            Paragraph::new("  (no worktree selected)").style(Style::new().fg(Color::DarkGray)),
            area,
        );
        return;
    };
    let draft = app.ui.worktree_pane.draft.borrow();
    let preview = draft.preview();
    let mut lines = vec![
        Line::from(vec![Span::styled(
            format!("Worktree: {}", entry.name()),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]),
        Line::from(format!("Path:     {}", entry.path)),
        Line::from(format!("Branch:   {} ({})", entry.branch, entry.head)),
        Line::from(format!(
            "State:    {}",
            if entry.is_current {
                "current worktree"
            } else {
                "linked worktree"
            }
        )),
    ];
    match preview {
        Some(preview) => {
            lines.push(Line::from(format!("Merge:    {}", merge_label(preview))));
            lines.push(Line::from(format!(
                "Changes:  {} tracked dirty · {} untracked",
                preview.tracked_dirty, preview.untracked
            )));
            lines.push(Line::from(format!(
                "Ignored:  {}",
                ignored_summary(preview)
            )));
            lines.push(Line::from(format!("Use:      {}", use_state(preview))));
        }
        None => {
            let changes = if entry.dirty_summary.is_empty() {
                "clean".to_string()
            } else {
                entry.dirty_summary.clone()
            };
            lines.push(Line::from(format!("Changes:  {changes}")));
            // A read that is still out is what the host has not answered; a
            // read that ended without a preview says why below.
            if draft.preview_request().is_some() {
                lines.push(Line::from("Merge:    reading from the host…".to_string()));
            }
        }
    }
    if let Some(notice) = draft.notice() {
        lines.push(Line::from(Span::styled(
            format!("Notice:   {notice}"),
            Style::new().fg(Color::Yellow),
        )));
    }
    block_with_keys(
        f,
        area,
        lines,
        key_hint(&[
            ("Up/Down", "scroll"),
            ("e", "enter"),
            ("d", "remove"),
            ("Esc", "back"),
        ]),
        &app.ui.worktree_pane.scroll,
    );
}

/// The removal confirmation: the focused-button row, what the preview measured
/// would be discarded, the branch choice, and the host's last answer. The
/// confirmation submits nothing while a removal is already out.
fn draw_confirm(f: &mut Frame, area: Rect, app: &App) {
    let draft = app.ui.worktree_pane.draft.borrow();
    let focus = draft.focus();
    let mut lines = vec![tab_header(
        focus,
        &[
            (RemoveFocus::Cancel, "Cancel"),
            (RemoveFocus::Remove, "Remove worktree"),
        ],
    )];
    match draft.preview() {
        Some(preview) => {
            lines.push(Line::from(format!(
                "Worktree: {} ({})",
                preview.slug, preview.path
            )));
            lines.push(Line::from(format!(
                "Branch:   {} · {}",
                preview.branch,
                merge_label(preview)
            )));
            lines.push(Line::from(format!(
                "Changes:  {} tracked dirty · {} untracked",
                preview.tracked_dirty, preview.untracked
            )));
            lines.push(Line::from(format!(
                "Ignored:  {}",
                ignored_summary(preview)
            )));
            if preview.leased || preview.locked {
                lines.push(Line::from(Span::styled(
                    format!("Blocked:  {}", use_state(preview)),
                    Style::new().fg(Color::Yellow),
                )));
            }
            let discarded_commits = !preview.merged && draft.delete_unmerged_branch();
            lines.push(Line::from(Span::styled(
                format!(
                    "Removal:  {}",
                    branch_disposition(preview, draft.delete_unmerged_branch())
                ),
                if discarded_commits {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new()
                },
            )));
        }
        None => {
            // Only a read still out is "reading"; a read that ended without a
            // preview states its reason in the notice below.
            if draft.preview_request().is_some() {
                lines.push(Line::from("Reading what a removal would discard…"));
            }
        }
    }
    if draft.removing() {
        lines.push(Line::from(Span::styled(
            "Removing…",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(notice) = draft.notice() {
        lines.push(Line::from(Span::styled(
            notice,
            Style::new().fg(Color::Yellow),
        )));
    }
    let mut keys: Vec<(&str, &str)> = vec![
        ("Left/Right", "choose"),
        ("Up/Down", "scroll"),
        ("Enter", "confirm"),
    ];
    // A merged branch goes with the tree whatever the choice says, so the key
    // that changes the choice is not offered where it would change nothing.
    if draft.preview().is_none_or(|preview| !preview.merged) {
        keys.push(("b", "branch choice"));
    }
    keys.push(("r", "read again"));
    keys.push(("Esc", "back"));
    block_with_keys(
        f,
        area,
        lines,
        key_hint(&keys),
        &app.ui.worktree_pane.scroll,
    );
}

/// What the removal does with the worktree's branch. A merged branch is
/// deleted whatever the toggle says, because its commits are already in the
/// base; the toggle decides only whether an unmerged branch's commits survive.
/// A branch Git counted no commits for is not the same as one holding none, so
/// the row says what it does not know instead of counting to zero.
fn branch_disposition(preview: &WorktreePreviewView, delete_unmerged: bool) -> String {
    if preview.merged {
        let base = preview.base_branch.as_deref().unwrap_or("its base");
        return format!("deleted with the worktree, already merged into {base}");
    }
    let held = match preview.unmerged_commits {
        Some(_) => "unmerged commits included",
        None => "no count of what it holds",
    };
    if delete_unmerged {
        format!("deleted with the worktree, {held} (b to keep)")
    } else {
        format!("kept, {held} (b to drop)")
    }
}

/// How the worktree's branch stands against the branch it was forked from.
fn merge_label(preview: &WorktreePreviewView) -> String {
    let Some(base) = preview.base_branch.as_deref() else {
        return "the record names no base branch".to_string();
    };
    if preview.merged {
        return format!("merged into {base}");
    }
    match preview.unmerged_commits {
        Some(commits) => format!("unmerged: {} ahead of {base}", commits_label(commits)),
        None => format!("Git did not answer against {base}"),
    }
}

fn commits_label(commits: u64) -> String {
    if commits == 1 {
        "1 commit".to_string()
    } else {
        format!("{commits} commits")
    }
}

/// The ignored entries a removal would take with the tree: the build-output
/// directories Git listed by name, then the count no build-output name covers.
fn ignored_summary(preview: &WorktreePreviewView) -> String {
    let mut parts = Vec::new();
    if !preview.ignored_build_dirs.is_empty() {
        parts.push(format!(
            "{} (build output)",
            preview.ignored_build_dirs.join(", ")
        ));
    }
    if preview.ignored_unknown > 0 {
        parts.push(format!("{} other", preview.ignored_unknown));
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Whether a process or a Git lock holds the worktree, which refuses removal
/// until it goes.
fn use_state(preview: &WorktreePreviewView) -> &'static str {
    match (preview.leased, preview.locked) {
        (true, _) => "in use by a session",
        (_, true) => "locked in Git",
        _ => "free",
    }
}

/// Build the list rows (one per filtered worktree). The cursor is an index
/// into the session's worktree rows (not the filtered list); when search is
/// active the cursor's item may be filtered out, in which case no row is
/// selected.
fn worktree_items(
    entries: &[WorktreeEntry],
    filtered_idx: &[usize],
    cur: usize,
) -> Vec<ListItem<'static>> {
    filtered_idx
        .iter()
        .map(|&idx| {
            let e = &entries[idx];
            let is_cursor = idx == cur;
            let cur_marker = if e.is_current {
                " * "
            } else if !e.dirty_summary.is_empty() {
                " ! "
            } else {
                "   "
            };
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
            if e.last_modified > 0 {
                spans.push(Span::styled(
                    format!(" {}", relative_time(now_epoch_secs(), e.last_modified)),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            if e.is_current {
                spans.push(Span::styled("  (current)", Style::new().fg(Color::Green)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect()
}

#[cfg(test)]
mod tests;
