//! The checkpoints tab at its list and detail levels: the recorded writes as
//! rows, and one write with the two sides the record kept set beside each
//! other.

use std::collections::HashSet;

use houyicoder_protocol::frontend::checkpoint::{
    CheckpointListView, CheckpointSide, CheckpointState, CheckpointTextView, CheckpointView,
    UnreadableView,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::{CheckpointRow, TABS, row_at, row_count, target_name, written_label};
use crate::state::App;
use crate::view::diff::changed_lines;
use crate::view::line_wrap::{elide_start, pad_width, truncate_width};
use crate::view::navigation::{block_with_keys, key_hint, key_row_height, tab_header};
use crate::view::relative_time::{now_epoch_secs, relative_time};

/// Rows the notice band occupies above the listing. It is held open whether or
/// not a notice is showing: the band sits above the list, so letting it appear
/// on the frame an answer lands would move every row down, and the frame after
/// a notice clears would move them back.
const NOTICE_BAND: u16 = 2;

/// Columns the listing spends on the cursor marker before a row starts.
const MARKER_COLUMNS: usize = 2;

/// Columns a row spends stating whether the write landed.
const STATE_COLUMNS: usize = 12;

/// Columns a row spends on when the write landed, or on an unreadable
/// record's name.
const SECOND_COLUMNS: usize = 12;

/// Columns a row keeps for its target before the second column is given up.
/// Below this the target would show only the directories its sibling rows
/// share, and the rows could no longer be told apart.
const TARGET_FLOOR: usize = 20;

/// The recorded writes, one row each, with the state the host judged and when
/// the write landed. A refusal or a failed read sits above the rows, because
/// the reason a key did nothing has to be readable where the key was pressed.
pub(super) fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let listing = pane.checkpoints();
    let notice = pane.notice();
    let rows = listing.as_ref().map(row_count).unwrap_or(0);
    let footer = list_footer(listing.as_ref(), pane.cursor.get());
    // The notice band is reserved whether or not a notice is showing. It sits
    // above the list, so letting it appear on the frame an answer lands would
    // move every row down, and the frame after a notice clears would move them
    // back. A band that is always there costs one blank row and keeps the
    // listing still across the whole read.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(NOTICE_BAND),
            Constraint::Min(0),
            Constraint::Length(key_row_height(&footer, area)),
        ])
        .split(area);
    let mut header = vec![tab_header(pane.tab.get(), &TABS)];
    // The count keeps its place from the first frame. Reading it off the
    // listing only would add a segment when the answer lands, and the tab
    // header would reflow underneath the cursor.
    header[0].spans.push(Span::styled(
        format!("   {rows} recorded"),
        Style::new().fg(Color::DarkGray),
    ));
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
            "The host has not answered; reopening the pane reads again"
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
        let cursor = pane.cursor.get().min(rows - 1);
        let items = list_items(list, chunks[2].width, cursor);
        let mut state = ListState::default();
        state.select(Some(cursor));
        f.render_stateful_widget(
            List::new(items).highlight_symbol("\u{276f} "),
            chunks[2],
            &mut state,
        );
    }
    render_keys(f, chunks[3], footer);
}

/// The keys that act on this listing. Selecting and opening need a row to act
/// on, and restoring needs one a restore could be measured against. An empty
/// list and a row the host could not judge leave both groups off, since the
/// row on screen is not one the keys can act on.
fn list_footer(listing: Option<&CheckpointListView>, cursor: usize) -> Line<'static> {
    let mut keys: Vec<(&'static str, &'static str)> = Vec::new();
    let row = listing
        .filter(|list| row_count(list) > 0)
        .and_then(|list| row_at(list, cursor));
    if let Some(CheckpointRow::Write(view)) = row {
        keys.push(("Up/Down", "select"));
        keys.push(("Enter", "open"));
        if restorable(view) {
            keys.push(("r", "restore"));
        }
    }
    keys.push(("Left/Right", "switch tab"));
    keys.push(("Esc", "close"));
    key_hint(&keys)
}

/// One write's detail: the file it targeted and when, the state its target is
/// in, and the two sides the record kept set beside each other.
pub(super) fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let listing = pane.checkpoints();
    let index = pane.cursor.get();
    let row = listing.as_ref().and_then(|list| row_at(list, index));
    let superseded = listing
        .as_ref()
        .and_then(|list| superseded_rows(list).get(index).copied())
        .unwrap_or(false);
    let texts = pane.texts();
    let (mut lines, openable) = match row {
        Some(CheckpointRow::Write(view)) => {
            let mut lines = vec![tab_header(pane.tab.get(), &TABS)];
            let shown = texts
                .as_ref()
                .filter(|shown| shown.operation == view.operation);
            lines.extend(write_lines(view, superseded, shown, pane.reading_texts()));
            // A record whose earlier bytes were never kept has nothing to put
            // back, so its detail offers no key for it. The state row already
            // says which record this is.
            (lines, restorable(view))
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
    let mut keys: Vec<(&'static str, &'static str)> = Vec::new();
    if openable {
        keys.push(("r", "restore"));
    }
    keys.push(("Esc", "back"));
    block_with_keys(f, area, lines, &keys, &pane.scroll);
}

/// A write the host judged, as the detail shows it. The title names the file
/// and when the write landed, which is how a user tells one record from
/// another; the name the record is stored under stays off the screen.
fn write_lines(
    view: &CheckpointView,
    superseded: bool,
    texts: Option<&CheckpointTextView>,
    reading: bool,
) -> Vec<Line<'static>> {
    let (label, color) = state_label(view.state, superseded);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                target_name(&view.target),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" \u{00b7} {}", written_label(view.written_ms)),
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("State:      "),
            Span::styled(label, Style::new().fg(color)),
        ]),
        Line::from(Span::styled(
            format!("Target:     {}", view.target),
            Style::new().fg(Color::DarkGray),
        )),
    ];
    match texts {
        Some(texts) => lines.extend(side_lines(texts)),
        None if reading => lines.push(Line::from(Span::styled(
            "Reading both sides from the host…",
            Style::new().fg(Color::DarkGray),
        ))),
        None => lines.push(Line::from(Span::styled(
            "Both sides are not shown; reopening this row reads them again",
            Style::new().fg(Color::DarkGray),
        ))),
    }
    lines
}

/// Both sides as the detail compares them: the lines that left and the lines
/// that took their place, and a row naming any side that could not be set
/// beside the other. A side the record kept no bytes for is said rather than
/// drawn as an empty file, because an empty file is a different fact.
///
/// A side that cannot be compared in full stops the comparison altogether.
/// Two halves of a large file differ at the cut even when the file did not
/// change there, and a side with no kept bytes would make every line of the
/// other side read as if it had just arrived.
fn side_lines(texts: &CheckpointTextView) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut compares = true;
    for (name, side) in [("Before", &texts.before), ("After", &texts.after)] {
        match side {
            CheckpointSide::NotKept => {
                compares = false;
                lines.push(note(format!("{name}: the record kept none of these bytes")))
            }
            CheckpointSide::NotText => {
                compares = false;
                lines.push(note(format!(
                    "{name}: these bytes are not text, so the sides cannot be compared line by line"
                )))
            }
            CheckpointSide::Text {
                truncated: true, ..
            } => {
                compares = false;
                lines.push(note(format!(
                    "{name}: the file is larger than this view holds, so the sides are not compared"
                )))
            }
            CheckpointSide::Text { .. } | CheckpointSide::Absent => {}
        }
    }
    if !compares {
        return lines;
    }
    let changed = changed_lines(side_text(&texts.before), side_text(&texts.after));
    if changed.is_empty() {
        lines.push(note("Neither side held anything to show".to_string()));
    }
    lines.extend(changed);
    lines
}

/// The text one side holds, when it holds text. An absent side, a side the
/// record kept no bytes for, and one holding bytes that are not text all
/// contribute no line to the comparison.
fn side_text(side: &CheckpointSide) -> Option<&str> {
    match side {
        CheckpointSide::Text { text, .. } => Some(text),
        CheckpointSide::Absent | CheckpointSide::NotKept | CheckpointSide::NotText => None,
    }
}

/// A row that explains a side of the comparison rather than showing one.
fn note(text: String) -> Line<'static> {
    Line::from(Span::styled(text, Style::new().fg(Color::DarkGray)))
}

/// A record the host could not judge, as the detail shows it.
fn unreadable_lines(view: &UnreadableView) -> Vec<Line<'static>> {
    let name = view.operation.as_deref().unwrap_or("(unnamed record)");
    vec![
        Line::from(vec![Span::styled(
            name.to_string(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]),
        Line::from(Span::styled(
            format!("Unreadable: {}", view.reason),
            Style::new().fg(Color::Yellow),
        )),
        Line::from("Nothing was put back: the host cannot judge this record"),
    ]
}

/// The state a write's target is in, in the words the user acts on. A write a
/// later write to the same file left behind is superseded whatever its own
/// state says: the target now holds the later write's bytes.
fn state_label(state: CheckpointState, superseded: bool) -> (&'static str, Color) {
    if superseded {
        return ("superseded", Color::DarkGray);
    }
    match state {
        CheckpointState::Applied => ("active", Color::Green),
        CheckpointState::NotApplied => ("reverted", Color::DarkGray),
        CheckpointState::Conflict => ("modified", Color::Yellow),
        CheckpointState::Unverified => ("unverified", Color::Yellow),
    }
}

/// Whether a restore can act on this record. The host puts the earlier bytes
/// back, and a record that kept none of them has nothing to put.
fn restorable(view: &CheckpointView) -> bool {
    view.state != CheckpointState::Unverified
}

/// Which writes a later write to the same target left behind. A later write
/// that was itself undone hands the file back, so it supersedes nothing; a
/// later write that still stands supersedes every write before it.
fn superseded_rows(list: &CheckpointListView) -> Vec<bool> {
    let mut standing: HashSet<&str> = HashSet::new();
    let mut superseded = vec![false; list.checkpoints.len()];
    for (index, view) in list.checkpoints.iter().enumerate().rev() {
        superseded[index] = standing.contains(view.target.as_str());
        if view.state != CheckpointState::NotApplied {
            standing.insert(view.target.as_str());
        }
    }
    superseded
}

fn list_items(list: &CheckpointListView, width: u16, cursor: usize) -> Vec<ListItem<'static>> {
    let now = now_epoch_secs();
    let columns = RowColumns::for_width(width);
    let superseded = superseded_rows(list);
    let mut items: Vec<ListItem<'static>> = list
        .checkpoints
        .iter()
        .enumerate()
        .map(|(index, view)| {
            let mark = RowMark {
                selected: index == cursor,
                superseded: superseded[index],
                now,
            };
            ListItem::new(write_row(view, mark, columns))
        })
        .collect();
    items.extend(list.unreadable.iter().enumerate().map(|(offset, view)| {
        let mark = RowMark {
            selected: list.checkpoints.len() + offset == cursor,
            superseded: false,
            now,
        };
        ListItem::new(unreadable_row(view, mark, columns))
    }));
    items
}

/// What the listing knows about a row beyond the record itself: whether it is
/// under the cursor, whether a later write left it behind, and the clock the
/// age is counted against.
#[derive(Clone, Copy)]
struct RowMark {
    selected: bool,
    superseded: bool,
    now: u64,
}

/// What each part of a row may take. The listing draws the cursor marker in
/// front of every row, so a row has that much less room than the list has,
/// and one without the room for a second column drops it rather than cut the
/// target down to the prefix every row shares.
#[derive(Clone, Copy)]
struct RowColumns {
    second: Option<usize>,
    target: usize,
}

impl RowColumns {
    fn for_width(width: u16) -> Self {
        let room = usize::from(width).saturating_sub(MARKER_COLUMNS);
        let second =
            (room >= STATE_COLUMNS + SECOND_COLUMNS + TARGET_FLOOR).then_some(SECOND_COLUMNS);
        let target = room.saturating_sub(STATE_COLUMNS + second.unwrap_or(0));
        Self { second, target }
    }
}

fn write_row(view: &CheckpointView, mark: RowMark, columns: RowColumns) -> Line<'static> {
    let (label, color) = state_label(view.state, mark.superseded);
    let mut spans = vec![Span::styled(
        format!("{label:<STATE_COLUMNS$}"),
        Style::new().fg(color),
    )];
    if let Some(width) = columns.second {
        let when = match view.written_ms {
            Some(ms) => relative_time(mark.now, ms / 1_000),
            None => "never landed".to_string(),
        };
        spans.push(Span::styled(
            cell_text(&when, width),
            Style::new().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        elide_start(&view.target, columns.target),
        target_style(mark.selected),
    ));
    Line::from(spans)
}

fn unreadable_row(view: &UnreadableView, mark: RowMark, columns: RowColumns) -> Line<'static> {
    let name = view.operation.as_deref().unwrap_or("(unnamed record)");
    let mut spans = vec![Span::styled(
        cell_text("unreadable", STATE_COLUMNS),
        Style::new().fg(Color::Yellow),
    )];
    if let Some(width) = columns.second {
        spans.push(Span::styled(
            cell_text(name, width),
            Style::new().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        elide_start(&view.reason, columns.target),
        target_style(mark.selected),
    ));
    Line::from(spans)
}

/// The target under the cursor is the one thing the cursor recolours, so the
/// row keeps the colour of its state and the selection adds weight to the
/// part the user is choosing.
fn target_style(selected: bool) -> Style {
    if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    }
}

/// A value held to a fixed column count, so the column after it starts at the
/// same place on every row. A value longer than the column is cut with an
/// ellipsis instead of pushing the columns that follow out of line.
fn cell_text(value: &str, width: usize) -> String {
    pad_width(&truncate_width(value, width), width)
}

fn render_keys(f: &mut Frame, area: Rect, keys: Line) {
    f.render_widget(
        Paragraph::new(keys)
            .wrap(Wrap { trim: false })
            .style(Style::new().fg(Color::DarkGray)),
        area,
    );
}
