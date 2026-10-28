//! The /sandbox pane at its two tabs and three levels: what confines the
//! session, the writes the session recorded, one write's detail, and the
//! confirmation that puts its earlier bytes back.
//!
//! The listing is one row per recorded write followed by one row per record
//! the host could not judge, so the cursor index, the scrolling list, and the
//! two levels below it all count the same rows. A write the host could not
//! judge is listed with its reason instead of being left out or guessed at.

use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointState, CheckpointView, FenceState, RestoreView,
    UnreadableView,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::state::App;
use crate::ui::{CheckpointLevel, RestoreFocus, SandboxTab};
use crate::view::navigation::{block_with_keys, key_hint, key_row_height, tab_header};
use crate::view::relative_time::{now_epoch_secs, relative_time};

/// The two tabs, in the order the left and right keys walk them.
const TABS: [(SandboxTab, &str); 2] = [
    (SandboxTab::Boundaries, "Boundaries"),
    (SandboxTab::Checkpoints, "Checkpoints"),
];

/// One row of the checkpoints listing: a write the host judged, or a record it
/// could not judge and why.
#[derive(Clone, Copy)]
pub(crate) enum CheckpointRow<'a> {
    /// A recorded write.
    Write(&'a CheckpointView),
    /// A record the host could not judge.
    Unreadable(&'a UnreadableView),
}

/// How many rows the listing holds, which the cursor and the renderer both
/// count.
pub(crate) fn row_count(list: &CheckpointListView) -> usize {
    list.checkpoints.len() + list.unreadable.len()
}

/// The row at an index of the listing, in the order the list draws them.
pub(crate) fn row_at(list: &CheckpointListView, index: usize) -> Option<CheckpointRow<'_>> {
    match list.checkpoints.get(index) {
        Some(view) => Some(CheckpointRow::Write(view)),
        None => list
            .unreadable
            .get(index.checked_sub(list.checkpoints.len())?)
            .map(CheckpointRow::Unreadable),
    }
}

/// What a restore answer says, wherever the pane shows it.
pub(crate) fn restore_sentence(operation: &str, outcome: &RestoreView) -> String {
    match outcome {
        RestoreView::Unchanged => format!("checkpoint: {operation} already held the earlier bytes"),
        RestoreView::Restored => format!("checkpoint: restored {operation}"),
        RestoreView::Kept { copy: Some(copy) } => format!(
            "checkpoint: the target changed since the write; the earlier bytes went to {copy}"
        ),
        RestoreView::Kept { copy: None } => {
            "checkpoint: the target changed since the write, which created it; nothing was put back"
                .to_string()
        }
        RestoreView::Unbacked => {
            format!("checkpoint: the earlier bytes of {operation} were never kept")
        }
    }
}

pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    match (pane.tab.get(), pane.level.get()) {
        (SandboxTab::Boundaries, _) => draw_boundaries(f, area, app),
        (SandboxTab::Checkpoints, CheckpointLevel::Detail) => draw_detail(f, area, app),
        (SandboxTab::Checkpoints, CheckpointLevel::Restore) => draw_restore(f, area, app),
        (SandboxTab::Checkpoints, CheckpointLevel::List) => draw_list(f, area, app),
    }
}

/// What confines the session. A fence the host could not set up states its
/// reason rather than reading as a healthy boundary, and a session with no
/// fence says so on the row where the user looks for the answer.
fn draw_boundaries(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let mut lines = vec![tab_header(pane.tab.get(), &TABS)];
    match pane.boundaries() {
        Some(view) => {
            let (label, color) = fence_label(&view.fence);
            lines.push(Line::from(vec![
                Span::raw("Fence:     "),
                Span::styled(label, Style::new().fg(color)),
            ]));
            lines.push(Line::from(format!("Workspace: {}", view.workspace_root)));
            if view.writable_roots.is_empty() {
                lines.push(Line::from("Writable:  nothing outside the workspace"));
            } else {
                let mut roots = view.writable_roots.iter();
                if let Some(first) = roots.next() {
                    lines.push(Line::from(format!("Writable:  {first}")));
                }
                for root in roots {
                    lines.push(Line::from(format!("           {root}")));
                }
            }
        }
        None if pane.reading_boundaries() => {
            lines.push(Line::from("Reading the boundaries from the host…"));
        }
        None => lines.push(Line::from(
            "The host has not answered; r reads the boundaries again",
        )),
    }
    if let Some(notice) = pane.notice() {
        lines.push(Line::from(Span::styled(
            notice,
            Style::new().fg(Color::Yellow),
        )));
    }
    block_with_keys(
        f,
        area,
        lines,
        key_hint(&[
            ("Left/Right", "switch tab"),
            ("r", "read again"),
            ("p", "permissions"),
            ("Esc", "close"),
        ]),
        &pane.scroll,
    );
}

/// The fence's state in the words the user acts on. A fence that failed says
/// why, because the list of writable roots below then describes a boundary
/// nothing enforces.
fn fence_label(fence: &FenceState) -> (String, Color) {
    match fence {
        FenceState::Enforced => (
            "enforced: writes outside the roots below are refused".to_string(),
            Color::Green,
        ),
        FenceState::NotEnforced => (
            "not enforced: the session can write anywhere its user can".to_string(),
            Color::Yellow,
        ),
        FenceState::Failed(reason) => (format!("could not be set up: {reason}"), Color::Red),
        FenceState::Unavailable => (
            "not offered on this platform: writes follow its permission".to_string(),
            Color::Yellow,
        ),
    }
}

/// The recorded writes, one row each, with the state the host judged and when
/// the write landed. A refusal or a failed read sits above the rows, because
/// the reason a key did nothing has to be readable where the key was pressed.
fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let listing = pane.checkpoints();
    let notice = pane.notice();
    let rows = listing.as_ref().map(row_count).unwrap_or(0);
    let footer = key_hint(&[
        ("Up/Down", "select"),
        ("Enter", "open"),
        ("r", "restore"),
        ("Left/Right", "switch tab"),
        ("Esc", "close"),
    ]);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if notice.is_some() { 2 } else { 0 }),
            Constraint::Min(0),
            Constraint::Length(key_row_height(&footer, area)),
        ])
        .split(area);
    let mut header = vec![tab_header(pane.tab.get(), &TABS)];
    if listing.is_some() {
        header[0].spans.push(Span::styled(
            format!("   {rows} recorded"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(header), chunks[0]);
    if let Some(notice) = notice {
        f.render_widget(
            Paragraph::new(notice)
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(Color::Yellow)),
            chunks[1],
        );
    }
    let Some(list) = listing.as_ref() else {
        let message = if pane.reading_checkpoints() {
            "Reading the recorded writes from the host…"
        } else {
            "The host has not answered; r reads the checkpoints again"
        };
        f.render_widget(
            Paragraph::new(message).style(Style::new().fg(Color::DarkGray)),
            chunks[2],
        );
        render_keys(f, chunks[3], footer);
        return;
    };
    if rows == 0 {
        f.render_widget(
            Paragraph::new("  (no file writes were recorded in this session)")
                .style(Style::new().fg(Color::DarkGray)),
            chunks[2],
        );
    } else {
        let items = list_items(list);
        let mut state = ListState::default();
        state.select(Some(pane.cursor.get().min(rows - 1)));
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
    render_keys(f, chunks[3], footer);
}

/// One write's detail: what it targeted, what the target holds now, when it
/// landed, and both images the record kept.
fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let listing = pane.checkpoints();
    let row = listing
        .as_ref()
        .and_then(|list| row_at(list, pane.cursor.get()));
    let (mut lines, openable) = match row {
        Some(CheckpointRow::Write(view)) => {
            let mut lines = vec![tab_header(pane.tab.get(), &TABS)];
            lines.extend(write_lines(view));
            (lines, true)
        }
        Some(CheckpointRow::Unreadable(view)) => {
            let mut lines = vec![tab_header(pane.tab.get(), &TABS)];
            lines.extend(unreadable_lines(view));
            (lines, false)
        }
        None => (
            vec![
                tab_header(pane.tab.get(), &TABS),
                Line::from("(no checkpoint selected)"),
            ],
            false,
        ),
    };
    if let Some(notice) = pane.notice() {
        lines.push(Line::from(Span::styled(
            notice,
            Style::new().fg(Color::Yellow),
        )));
    }
    let mut keys = vec![("Up/Down", "scroll")];
    if openable {
        keys.push(("r", "restore"));
    }
    keys.push(("Esc", "back"));
    block_with_keys(f, area, lines, key_hint(&keys), &pane.scroll);
}

/// A write the host judged, as the detail shows it.
fn write_lines(view: &CheckpointView) -> Vec<Line<'static>> {
    let (label, color) = state_label(view.state);
    vec![
        Line::from(vec![Span::styled(
            format!("Checkpoint: {}", view.operation),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]),
        Line::from(format!("Target:     {}", view.target)),
        Line::from(vec![
            Span::raw("State:      "),
            Span::styled(label, Style::new().fg(color)),
        ]),
        Line::from(format!("Written:    {}", written_label(view.written_ms))),
        Line::from(format!("Before:     {}", image_label(&view.before))),
        Line::from(format!("After:      {}", image_label(&view.after))),
    ]
}

/// A record the host could not judge, as the detail shows it.
fn unreadable_lines(view: &UnreadableView) -> Vec<Line<'static>> {
    let name = view.operation.as_deref().unwrap_or("(unnamed record)");
    vec![
        Line::from(vec![Span::styled(
            format!("Checkpoint: {name}"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]),
        Line::from(Span::styled(
            format!("Unreadable: {}", view.reason),
            Style::new().fg(Color::Yellow),
        )),
        Line::from("Nothing was put back: the host cannot judge this record"),
    ]
}

/// The restore confirmation: the focused buttons, what the restore will do to
/// this target, and the last answer the host gave. The buttons start on the
/// one that changes nothing.
fn draw_restore(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let listing = pane.checkpoints();
    let row = listing
        .as_ref()
        .and_then(|list| row_at(list, pane.cursor.get()));
    let mut lines = vec![tab_header(
        pane.focus.get(),
        &[
            (RestoreFocus::Cancel, "Cancel"),
            (RestoreFocus::Restore, "Restore checkpoint"),
        ],
    )];
    match row {
        Some(CheckpointRow::Write(view)) => {
            lines.push(Line::from(format!(
                "Checkpoint: {} ({})",
                view.operation, view.target
            )));
            lines.push(Line::from(format!(
                "Before:     {}",
                image_label(&view.before)
            )));
            lines.push(Line::from(restore_effect(view)));
        }
        _ => lines.push(Line::from("(no checkpoint selected)")),
    }
    if pane.restore_request().is_some() {
        lines.push(Line::from(Span::styled(
            "Restoring…",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(notice) = pane.notice() {
        lines.push(Line::from(Span::styled(
            notice,
            Style::new().fg(Color::Yellow),
        )));
    }
    block_with_keys(
        f,
        area,
        lines,
        key_hint(&[
            ("Left/Right", "choose"),
            ("Up/Down", "scroll"),
            ("Enter", "confirm"),
            ("Esc", "back"),
        ]),
        &pane.scroll,
    );
}

/// What a restore does to this target, said before the user commits to it. A
/// target something else changed keeps its current bytes, so the sentence names
/// where the earlier ones go instead of promising an overwrite. A file the
/// write created has no earlier bytes: putting it back removes it, and the
/// sentence says so rather than describing a restore that would not happen.
fn restore_effect(view: &CheckpointView) -> String {
    if !view.before.present {
        return match view.state {
            CheckpointState::Applied => {
                "Restore:    removes the file this write created; there was nothing there before"
                    .to_string()
            }
            CheckpointState::NotApplied => {
                "Restore:    changes nothing; the file this write created is already gone"
                    .to_string()
            }
            CheckpointState::Conflict => {
                "Restore:    the target changed since the write, so it keeps its bytes, and there is nothing earlier to put beside it"
                    .to_string()
            }
            CheckpointState::Unverified => {
                "Restore:    nothing to put back; the earlier bytes were never kept".to_string()
            }
        };
    }
    match view.state {
        CheckpointState::Applied => {
            "Restore:    puts the bytes from Before back into the target".to_string()
        }
        CheckpointState::NotApplied => {
            "Restore:    changes nothing; the target already holds Before".to_string()
        }
        CheckpointState::Conflict => {
            "Restore:    the target changed since the write, so it keeps its bytes and Before goes to a copy beside it"
                .to_string()
        }
        CheckpointState::Unverified => {
            "Restore:    nothing to put back; the earlier bytes were never kept".to_string()
        }
    }
}

/// The state a write's target is in, in the words the user acts on.
fn state_label(state: CheckpointState) -> (&'static str, Color) {
    match state {
        CheckpointState::Applied => ("ready", Color::Green),
        CheckpointState::NotApplied => ("not applied", Color::DarkGray),
        CheckpointState::Conflict => ("conflict", Color::Yellow),
        CheckpointState::Unverified => ("unverified", Color::Yellow),
    }
}

/// One side of a write, as a sentence. A file that was not there says so; a
/// side whose bytes were not kept says that rather than naming no size.
fn image_label(image: &CheckpointImage) -> String {
    if !image.present {
        return "absent".to_string();
    }
    match (&image.hash, image.bytes) {
        (Some(hash), Some(bytes)) => format!("{bytes} bytes, kept as {hash}"),
        (Some(hash), None) => format!("kept as {hash}"),
        (None, Some(bytes)) => format!("{bytes} bytes, not kept"),
        (None, None) => "not kept".to_string(),
    }
}

/// When a write landed, relative to now. A record that never landed has no
/// time, which the row states instead of dating to the epoch.
fn written_label(written_ms: Option<u64>) -> String {
    match written_ms {
        Some(ms) => relative_time(now_epoch_secs(), ms / 1_000),
        None => "the write never landed".to_string(),
    }
}

fn list_items(list: &CheckpointListView) -> Vec<ListItem<'static>> {
    let now = now_epoch_secs();
    let mut items: Vec<ListItem<'static>> = list
        .checkpoints
        .iter()
        .map(|view| ListItem::new(write_row(view, now)))
        .collect();
    items.extend(
        list.unreadable
            .iter()
            .map(|view| ListItem::new(unreadable_row(view))),
    );
    items
}

fn write_row(view: &CheckpointView, now: u64) -> Line<'static> {
    let (label, color) = state_label(view.state);
    let when = match view.written_ms {
        Some(ms) => relative_time(now, ms / 1_000),
        None => "never landed".to_string(),
    };
    Line::from(vec![
        Span::styled(format!("{label:<14}"), Style::new().fg(color)),
        Span::styled(format!("{when:<10}"), Style::new().fg(Color::DarkGray)),
        Span::styled(view.target.clone(), Style::new().fg(Color::White)),
    ])
}

fn unreadable_row(view: &UnreadableView) -> Line<'static> {
    let name = view.operation.as_deref().unwrap_or("(unnamed record)");
    Line::from(vec![
        Span::styled(
            format!("{:<14}", "unreadable"),
            Style::new().fg(Color::Yellow),
        ),
        Span::styled(format!("{name:<10}"), Style::new().fg(Color::DarkGray)),
        Span::styled(view.reason.clone(), Style::new().fg(Color::DarkGray)),
    ])
}

fn render_keys(f: &mut Frame, area: Rect, keys: Line) {
    f.render_widget(
        Paragraph::new(keys)
            .wrap(Wrap { trim: false })
            .style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

#[cfg(test)]
mod tests;
