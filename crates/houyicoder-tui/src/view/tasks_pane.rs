//! /tasks pane content: every background command the session started, with a
//! detail drill-down. The list groups the commands still running above the
//! ones that ended, newest first in each group, and shows each task's number,
//! state, run time, and command. Enter opens one task: a running one shows
//! its latest output lines and is re-read while it runs, and an ended one
//! shows the output it produced. The detail keeps its state and key hints
//! fixed and scrolls only the output.

pub(crate) mod rows;
pub(crate) mod state;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskDetail, BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};
use houyicoder_protocol::tool::strip_ansi;

use crate::state::App;
use crate::tasks_state::TaskDetail;
use crate::view::components::section_list_body;
use crate::view::line_wrap::cursor_window_offset;
use crate::view::line_wrap::max_scroll_offset;
use crate::view::line_wrap::truncate_width;
use crate::view::navigation::key_hint;
use crate::view::relative_time::summary_span_label;
use crate::view::tasks_pane::rows::{TaskRow, task_ids, task_rows};

/// Rows the pane reserves when opened inline by the slash command or by Down
/// on an empty input.
pub(crate) const TASKS_PANE_HEIGHT: u16 = 16;

/// The pane's content: the open detail, otherwise the list.
pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    if let Some(detail) = app.ui.tasks_pane.detail() {
        draw_detail(f, inner, app, detail);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    let rows = task_rows(app.sessions.current.capabilities.tasks.iter());
    f.render_widget(Paragraph::new(head_line(&rows)), chunks[0]);

    let ids = task_ids(&rows);
    let selected = app.ui.tasks_pane.selected_row(&ids).unwrap_or(0);
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new("No background commands.").style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let (body, selected_line) = task_lines(&rows, selected, chunks[1].width);
        let offset = cursor_window_offset(selected_line, body.len(), chunks[1].height);
        f.render_widget(Paragraph::new(body).scroll((offset, 0)), chunks[1]);
    }

    // The list offers only the actions the selected row can take: an empty
    // list has nothing to open or stop, and an ended task has nothing left to
    // stop.
    let mut hints = vec![("Up/Down", "select")];
    if !rows.is_empty() {
        hints.push(("Enter", "open"));
        if matches!(rows[selected], TaskRow::Running(_)) {
            hints.push(("k", "kill"));
        }
    }
    hints.push(("Esc", "close"));
    f.render_widget(
        Paragraph::new(key_hint(&hints)).style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

/// The title and, beside it, how many commands still run and how many ended.
/// The counts share the title line so the body keeps every row the pane can
/// draw.
fn head_line(rows: &[TaskRow<'_>]) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "Tasks",
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )];
    let counts = count_line(rows);
    if !counts.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(counts, Style::new().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// How many commands still run and how many ended. An empty list states
/// nothing here; the body below already says the session started no
/// commands.
fn count_line(rows: &[TaskRow<'_>]) -> String {
    let running = rows
        .iter()
        .filter(|row| matches!(row, TaskRow::Running(_)))
        .count();
    let ended = rows.len() - running;
    let mut parts: Vec<String> = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if ended > 0 {
        parts.push(format!("{ended} ended"));
    }
    parts.join(" \u{00b7} ")
}

/// The list body: a heading per section, then one row per task with the
/// cursor marker on the selected row. The state and time columns are as wide
/// as their widest cell, so a row's columns stay aligned whatever endings and
/// durations the list holds, and the command takes what width remains, elided
/// when it does not fit rather than running past the pane.
fn task_lines(rows: &[TaskRow<'_>], cursor: usize, width: u16) -> (Vec<Line<'static>>, usize) {
    let tasks: Vec<&BackgroundTaskSnapshot> = rows.iter().map(TaskRow::task).collect();
    let labels: Vec<String> = tasks.iter().map(|task| task.id.label()).collect();
    let states: Vec<String> = tasks.iter().map(|task| status_word(&task.status)).collect();
    let durations: Vec<String> = tasks
        .iter()
        .map(|task| summary_span_label(task.elapsed_ms))
        .collect();
    let label_width = labels.iter().map(|label| label.len()).max().unwrap_or(0);
    let state_width = states.iter().map(|state| state.len()).max().unwrap_or(0);
    let duration_width = durations.iter().map(|cell| cell.len()).max().unwrap_or(0);
    // Marker, the three columns, and the gap before the command.
    let used = 2 + label_width + 2 + state_width + 2 + duration_width + 2;
    let command_width = usize::from(width).saturating_sub(used);

    let sections: Vec<&'static str> = rows.iter().map(TaskRow::section).collect();
    section_list_body(&sections, cursor, |index, line| {
        let task = tasks[index];
        line.push(Span::styled(
            format!("{:<label_width$}  ", labels[index]),
            Style::new().fg(Color::Yellow),
        ));
        line.push(Span::styled(
            format!("{:<state_width$}  ", states[index]),
            status_style(&task.status),
        ));
        line.push(Span::styled(
            format!("{:<duration_width$}  ", durations[index]),
            Style::new().fg(Color::DarkGray),
        ));
        line.push(Span::raw(truncate_width(
            &strip_ansi(&task.command),
            command_width,
        )));
    })
}

/// The words a row uses for a task state: the protocol's text form, with the
/// one state that reads as two words spelled out for a row.
fn status_word(status: &BackgroundTaskStatus) -> String {
    match status {
        BackgroundTaskStatus::TimedOut => "timed out".to_string(),
        other => other.as_str().to_string(),
    }
}

/// A running task reads as live, a clean exit as done, an ending the command
/// did not choose as a warning.
fn status_style(status: &BackgroundTaskStatus) -> Style {
    match status {
        BackgroundTaskStatus::Running => Style::new().fg(Color::Cyan),
        BackgroundTaskStatus::Finished => Style::new().fg(Color::Green),
        BackgroundTaskStatus::Failed => Style::new().fg(Color::Red),
        BackgroundTaskStatus::Killed | BackgroundTaskStatus::TimedOut => {
            Style::new().fg(Color::Yellow)
        }
        BackgroundTaskStatus::Unknown(_) => Style::new().fg(Color::DarkGray),
        _ => Style::new().fg(Color::DarkGray),
    }
}

/// The detail: identity and state rows, the scrollable output, and the key
/// hints. The hints drop the kill action once the task ended.
fn draw_detail(f: &mut Frame, inner: Rect, app: &App, detail: &TaskDetail) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let record = detail.record();
    f.render_widget(
        Paragraph::new(header_line(detail.id(), record, rows[0].width)),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(state_line(record)).style(Style::new().fg(Color::DarkGray)),
        rows[1],
    );

    let body = body_lines(record);
    let max_offset = max_scroll_offset(&body, rows[2].width, rows[2].height);
    app.ui.tasks_pane.set_detail_max_offset(max_offset);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((detail.offset().min(max_offset), 0)),
        rows[2],
    );
    let mut hints = vec![("Up/Down", "scroll")];
    if !record.is_some_and(|record| record.status.is_terminal()) {
        hints.push(("k", "kill"));
    }
    hints.push(("Esc", "back"));
    f.render_widget(
        Paragraph::new(key_hint(&hints)).style(Style::new().fg(Color::DarkGray)),
        rows[3],
    );
}

/// The identity row: the task number and its command line, elided to the
/// room the number leaves.
fn header_line(
    id: BackgroundTaskId,
    record: Option<&BackgroundTaskDetail>,
    width: u16,
) -> Line<'static> {
    let command = record.map_or("reading task...", |record| record.command.as_str());
    let label = id.label();
    let room = usize::from(width).saturating_sub(label.len() + 1);
    Line::from(vec![
        Span::styled(
            format!("{label} "),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::raw(truncate_width(&strip_ansi(command), room)),
    ])
}

/// Where the task stands, how long it ran, and how it exited once it has.
fn state_line(record: Option<&BackgroundTaskDetail>) -> String {
    let Some(record) = record else {
        return String::new();
    };
    let mut line = format!(
        "{} \u{00b7} {}",
        status_word(&record.status),
        summary_span_label(record.elapsed_ms)
    );
    if let Some(code) = record.exit_code {
        line.push_str(&format!(" \u{00b7} exit {code}"));
    }
    line
}

/// The output: standard output, then standard error under a label. A record
/// read before any output arrived falls back to its live tail.
fn body_lines(record: Option<&BackgroundTaskDetail>) -> Vec<Line<'static>> {
    let Some(record) = record else {
        return vec![Line::from(Span::styled(
            "reading...",
            Style::new().fg(Color::DarkGray),
        ))];
    };
    let mut lines = Vec::new();
    if !record.stdout.is_empty() {
        lines.extend(
            record
                .stdout
                .lines()
                .map(|line| Line::from(strip_ansi(line))),
        );
    }
    if !record.stderr.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "stderr",
            Style::new().fg(Color::DarkGray),
        )));
        lines.extend(
            record
                .stderr
                .lines()
                .map(|line| Line::from(strip_ansi(line))),
        );
    }
    if lines.is_empty() {
        lines.extend(record.tail.iter().map(|line| Line::from(strip_ansi(line))));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "no output yet",
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines
}

#[cfg(test)]
mod tests;
