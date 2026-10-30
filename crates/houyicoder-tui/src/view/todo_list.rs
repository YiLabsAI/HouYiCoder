//! Session checklist rendering — the collapsed list shown at the tail of the
//! transcript while a run is in flight or idle. The items render as plain,
//! selectable transcript rows so a drag can lift them into the clipboard like
//! any other content. The data is a wire-side view model the accumulator
//! parses from each todo-write tool call; this module only reads and renders
//! it.
//!
//! Collapsed (default): at most three visible items plus a one-line hidden
//! summary. Priority is the active item first (always slot 1), then recently
//! completed items within a 30-second TTL (so the user sees the green
//! checkmark appear before they collapse), then the next pending. Expanded
//! (toggled inline) renders items grouped by status, truncated by terminal
//! height with a hidden summary.
//!
//! Small-screen degradation: below 12 terminal rows the collapsed view shows
//! only the active item plus the hidden summary; below 8 rows only the summary
//! line survives. This keeps the interaction surface usable on tiny terminals.

use std::collections::HashMap;
use std::time::Instant;

use crate::state::App;
use crate::todo_view::RECENT_COMPLETION_TTL;
use crate::todo_view::{TodoStatus, TodoView};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Glyph and color for each checklist lifecycle state.
fn glyph_for(status: TodoStatus) -> (&'static str, Color, bool) {
    match status {
        TodoStatus::InProgress => ("◼", Color::Cyan, false),
        TodoStatus::Paused => ("Ⅱ", Color::Yellow, false),
        TodoStatus::Completed => ("✔", Color::Green, true),
        TodoStatus::Pending => ("◻", Color::DarkGray, false),
    }
}

/// Maximum visible items in the collapsed view on a normal-sized terminal.
/// Three slots: active (always first), a recent completed (progress signal),
/// and the next pending (what is coming up). Deliberately smaller than an
/// adaptive max(3, rows-14) — the checklist is a progress signal,
/// not a work surface; the expanded view covers full visibility.
const COLLAPSED_MAX: usize = 3;

/// Pick at most three items for the collapsed view, respecting the terminal
/// height budget. Priority: active item (always slot 1), then recently
/// completed items within the TTL window (most recent first), then the next
/// pending. The active item is always first so the spinner anchor never jumps.
fn visible_collapsed<'a>(
    todos: &'a [TodoView],
    completion_at: &HashMap<String, Instant>,
    max: usize,
) -> Vec<&'a TodoView> {
    let mut visible: Vec<&TodoView> = Vec::with_capacity(max);
    if max > 0
        && let Some(active) = todos.iter().find(|t| t.status == TodoStatus::InProgress)
    {
        visible.push(active);
    }
    if visible.len() < max
        && let Some(paused) = todos.iter().find(|t| t.status == TodoStatus::Paused)
    {
        visible.push(paused);
    }
    // Recent completions receive priority; older completions remain eligible
    // while unresolved work keeps the checklist visible.
    let now = Instant::now();
    let recent_done = todos.iter().rev().filter(|t| {
        t.status == TodoStatus::Completed
            && completion_at
                .get(&t.content)
                .is_some_and(|ts| now.duration_since(*ts) < RECENT_COMPLETION_TTL)
    });
    for done in recent_done {
        if visible.len() >= max {
            break;
        }
        visible.push(done);
    }
    if visible.len() < max
        && let Some(pending) = todos.iter().find(|t| t.status == TodoStatus::Pending)
    {
        visible.push(pending);
    }
    for done in todos {
        if done.status != TodoStatus::Completed
            || visible.iter().any(|shown| shown.content == done.content)
        {
            continue;
        }
        if visible.len() >= max {
            break;
        }
        visible.push(done);
    }
    visible
}

fn count_by_status(todos: &[TodoView], status: TodoStatus) -> usize {
    todos.iter().filter(|t| t.status == status).count()
}

/// The label an item shows: the active-form phrasing for the in-progress item
/// (falling back to the content), the plain content otherwise.
fn item_label(item: &TodoView) -> String {
    match item.status {
        TodoStatus::InProgress => item
            .active_form
            .clone()
            .unwrap_or_else(|| item.content.clone()),
        TodoStatus::Paused => format!("{} (paused)", item.content),
        TodoStatus::Pending | TodoStatus::Completed => item.content.clone(),
    }
}

/// Render one item line: glyph plus content (or the active form for the
/// in-progress item). Completed items are struck through and dimmed.
fn item_line(item: &TodoView) -> Line<'static> {
    let (glyph, color, strike) = glyph_for(item.status);
    let label = item_label(item);
    let mut style = Style::default().fg(color);
    if strike {
        style = style.add_modifier(Modifier::CROSSED_OUT | Modifier::DIM);
    } else if matches!(item.status, TodoStatus::Pending | TodoStatus::Paused) {
        style = style.add_modifier(Modifier::DIM);
    }
    Line::from(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(label, style),
    ])
}

/// The plain, selectable text of one item line: glyph plus label. Matches the
/// styled line column-for-column so a drag lifts the visible text.
fn item_plain(item: &TodoView) -> String {
    let (glyph, _, _) = glyph_for(item.status);
    format!("{glyph} {}", item_label(item))
}

/// The collapsed hidden-summary body, listing only the counts hidden beyond
/// the two visible slots.
fn hidden_summary_body(
    hidden_pending: usize,
    hidden_completed: usize,
    hidden_active: usize,
    hidden_paused: usize,
) -> String {
    // The footer counts the same states, in the same order and under the same
    // names as the header above it. Two vocabularies for one list make the
    // reader check whether the two lines are even describing the same thing,
    // when all the footer says is how many rows the fold is hiding.
    let mut parts: Vec<String> = Vec::new();
    if hidden_completed > 0 {
        parts.push(format!("+{hidden_completed} done"));
    }
    if hidden_active > 0 {
        parts.push(format!("+{hidden_active} in progress"));
    }
    if hidden_paused > 0 {
        parts.push(format!("+{hidden_paused} paused"));
    }
    if hidden_pending > 0 {
        parts.push(format!("+{hidden_pending} open"));
    }
    if parts.is_empty() {
        return String::new();
    }
    // The key is named here because this line is the only thing telling the
    // reader there is more to see. It is the shared expand chord, the same one
    // every other foldable block answers to.
    format!(" … hidden: {} ({})", parts.join(", "), EXPAND_HINT)
}

/// The chord every foldable block in the transcript answers to, named the way
/// the footer spells it.
const EXPAND_HINT: &str = "ctrl+o to expand";

/// The identity the checklist's summary row publishes for the fold chord and
/// the click hit-test. Fixed, because the session holds one checklist.
pub(crate) const TODO_FOLD_KEY: &str = "todos";

/// The collapsed hidden-summary footer line, dim.
fn hidden_summary(
    hidden_pending: usize,
    hidden_completed: usize,
    hidden_active: usize,
    hidden_paused: usize,
) -> Line<'static> {
    Line::from(Span::styled(
        hidden_summary_body(
            hidden_pending,
            hidden_completed,
            hidden_active,
            hidden_paused,
        ),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ))
}

/// Render the checklist as selectable transcript rows: one (plain text, styled
/// line) pair per row. The plain text drives selection and copy; the styled
/// line keeps the glyph colors and strikethrough on screen. Empty when there
/// is no checklist, so the caller adds no rows.
pub fn render_rows(app: &App) -> Vec<(String, Line<'static>)> {
    let todos = &app.sessions.current.todos.items;
    if todos.is_empty() {
        return Vec::new();
    }
    // Header counts the whole list while recent completion rows remain visible.
    let done = count_by_status(todos, TodoStatus::Completed);
    let active = count_by_status(todos, TodoStatus::InProgress);
    let paused = count_by_status(todos, TodoStatus::Paused);
    let open = count_by_status(todos, TodoStatus::Pending);
    // A state nobody is in is left out: a zero beside a count the reader is
    // checking makes the line longer without adding a fact. The order is the
    // one the list below is grouped by, and the footer uses the same names.
    let mut parts: Vec<String> = Vec::new();
    if done > 0 {
        parts.push(format!("{done} done"));
    }
    if active > 0 {
        parts.push(format!("{active} in progress"));
    }
    if paused > 0 {
        parts.push(format!("{paused} paused"));
    }
    if open > 0 {
        parts.push(format!("{open} open"));
    }
    let h = if parts.is_empty() {
        format!("{} tasks", todos.len())
    } else {
        format!("{} tasks ({})", todos.len(), parts.join(", "))
    };
    let mut out = vec![(
        h.clone(),
        Line::from(Span::styled(h, Style::default().fg(Color::DarkGray))),
    )];
    let term_rows = app.ui.frame.feedback.terminal_rows.get();
    let body = if app.sessions.current.todos.expanded {
        render_expanded(todos, term_rows)
    } else {
        render_collapsed(todos, &app.sessions.current.todos.completion_at, term_rows)
    };
    out.extend(body);
    out
}

/// Render the collapsed view: up to COLLAPSED_MAX items (degraded by terminal
/// height) plus a hidden summary footer.
fn render_collapsed(
    todos: &[TodoView],
    completion_at: &HashMap<String, Instant>,
    term_rows: u16,
) -> Vec<(String, Line<'static>)> {
    let max = collapsed_max(term_rows);
    if max == 0 {
        // Tiny terminal: only the aggregate lifecycle counts remain.
        let hp = count_by_status(todos, TodoStatus::Pending);
        let hc = count_by_status(todos, TodoStatus::Completed);
        let ha = count_by_status(todos, TodoStatus::InProgress);
        let hz = count_by_status(todos, TodoStatus::Paused);
        return vec![(
            hidden_summary_body(hp, hc, ha, hz),
            hidden_summary(hp, hc, ha, hz),
        )];
    }
    let visible = visible_collapsed(todos, completion_at, max);
    let mut out: Vec<(String, Line<'static>)> = visible
        .iter()
        .map(|t| (item_plain(t), item_line(t)))
        .collect();
    let vis = |s: TodoStatus| visible.iter().filter(|t| t.status == s).count();
    let hp = count_by_status(todos, TodoStatus::Pending).saturating_sub(vis(TodoStatus::Pending));
    let hc =
        count_by_status(todos, TodoStatus::Completed).saturating_sub(vis(TodoStatus::Completed));
    let ha =
        count_by_status(todos, TodoStatus::InProgress).saturating_sub(vis(TodoStatus::InProgress));
    let hz = count_by_status(todos, TodoStatus::Paused).saturating_sub(vis(TodoStatus::Paused));
    if hp + hc + ha + hz > 0 {
        out.push((
            hidden_summary_body(hp, hc, ha, hz),
            hidden_summary(hp, hc, ha, hz),
        ));
    }
    out
}

/// The rows the collapsed checklist drew, so the tail can tag the summary line
/// with its fold affordance. The summary is the line the user clicks to see
/// the hidden rows, which is the same gesture every other foldable block
/// answers to.
pub fn collapsed_summary_row(rows: &[(String, Line<'static>)]) -> Option<usize> {
    rows.iter()
        .position(|(plain, _)| plain.starts_with(" … hidden:"))
}

/// Render the expanded view: all items grouped by status, truncated by
/// terminal height with a hidden summary footer.
fn render_expanded(todos: &[TodoView], term_rows: u16) -> Vec<(String, Line<'static>)> {
    let mut grouped: Vec<&TodoView> = Vec::with_capacity(todos.len());
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::InProgress));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Paused));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Pending));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Completed));
    // Truncate by terminal height: leave room for the input box (3 rows),
    // status bar (1), and a blank spacer (1), clamped to a minimum of 3 so a
    // very small terminal still shows something.
    let budget = (term_rows as usize)
        .saturating_sub(5)
        .max(3)
        .min(grouped.len());
    let visible: Vec<&TodoView> = grouped.iter().take(budget).copied().collect();
    let hidden = grouped.len().saturating_sub(visible.len());
    let mut out: Vec<(String, Line<'static>)> = visible
        .iter()
        .map(|t| (item_plain(t), item_line(t)))
        .collect();
    if hidden > 0 {
        let vis = |s: TodoStatus| visible.iter().filter(|t| t.status == s).count();
        let hp =
            count_by_status(todos, TodoStatus::Pending).saturating_sub(vis(TodoStatus::Pending));
        let hc = count_by_status(todos, TodoStatus::Completed)
            .saturating_sub(vis(TodoStatus::Completed));
        let ha = count_by_status(todos, TodoStatus::InProgress)
            .saturating_sub(vis(TodoStatus::InProgress));
        let hz = count_by_status(todos, TodoStatus::Paused).saturating_sub(vis(TodoStatus::Paused));
        out.push((
            hidden_summary_body(hp, hc, ha, hz),
            hidden_summary(hp, hc, ha, hz),
        ));
    }
    out
}

/// How many items the collapsed view shows, degraded by terminal height.
/// Below 8 rows: only the hidden summary (max=0). Below 12 rows: only the
/// active item (max=1). Otherwise: the full COLLAPSED_MAX.
fn collapsed_max(term_rows: u16) -> usize {
    if term_rows < 8 {
        0
    } else if term_rows < 12 {
        1
    } else {
        COLLAPSED_MAX
    }
}

#[cfg(test)]
mod tests;
