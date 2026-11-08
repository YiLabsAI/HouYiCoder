//! The transcript row a user-typed shell command draws.
//!
//! The command and its outcome share one row: the prompt and command on the
//! left, the outcome on the right, so a reader compares verdicts down one
//! column. What the host kept of the output and where it kept it add tags
//! after the outcome.

use houyicoder_protocol::tool::strip_ansi;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::records::ToolOutcome;
use crate::records::TranscriptLine;
use crate::transcript::shell_result::ShellStorage;
use crate::view::line_wrap::truncate_width;
use crate::view::relative_time::format_span_ms;
use crate::view::{ERROR_RED, SUCCESS_GREEN};

/// Everything one command row reports, gathered so the layout function takes
/// one value rather than a list of loose arguments.
pub(super) struct ShellRow<'a> {
    pub command: &'a str,
    pub outcome: ToolOutcome,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub cancelling: bool,
    pub storage: Option<ShellStorage>,
}

impl<'a> ShellRow<'a> {
    /// The row for a shell command line, or None for any other line. The stop
    /// in progress comes from the run rather than the line, because the
    /// transcript does not change until the result lands.
    pub(super) fn from_line(line: &'a TranscriptLine, cancelling: bool) -> Option<Self> {
        let TranscriptLine::ShellCommand {
            command,
            outcome,
            duration_ms,
            exit_code,
            timed_out,
            storage,
            ..
        } = line
        else {
            return None;
        };
        Some(Self {
            command,
            outcome: *outcome,
            duration_ms: *duration_ms,
            exit_code: *exit_code,
            timed_out: *timed_out,
            cancelling,
            storage: *storage,
        })
    }

    /// The text that goes on the right of the row: the exit status and the
    /// measured duration once the call finished, or the state while it runs
    /// or when it never ran.
    fn status_text(&self) -> String {
        let verdict = match self.outcome {
            ToolOutcome::Running if self.cancelling => "stopping".to_string(),
            ToolOutcome::Running => "running".to_string(),
            ToolOutcome::Refused => "refused".to_string(),
            ToolOutcome::NotRun => "stopped".to_string(),
            ToolOutcome::Success => self
                .exit_code
                .map(|code| format!("exit {code}"))
                .unwrap_or_else(|| "done".to_string()),
            ToolOutcome::Error if self.timed_out => "timed out".to_string(),
            ToolOutcome::Error => self
                .exit_code
                .map(|code| format!("exit {code}"))
                .unwrap_or_else(|| "failed".to_string()),
        };
        match self.duration_ms {
            Some(ms) if self.outcome != ToolOutcome::Running => {
                format!("{verdict} · {}", format_span_ms(ms))
            }
            _ => verdict,
        }
    }
}

/// The styles of the verdict, keyed to what happened rather than to the
/// outcome alone: a timeout, a stop in progress, and a refusal are states
/// the user can still act on, so they read yellow, while a command that
/// failed on its own reads red.
fn status_style(outcome: ToolOutcome, timed_out: bool, cancelling: bool) -> Style {
    let dim = Style::new().fg(Color::DarkGray);
    let yellow = Style::new().fg(Color::Yellow);
    match outcome {
        ToolOutcome::Success => Style::new().fg(SUCCESS_GREEN),
        ToolOutcome::Error if timed_out => yellow,
        ToolOutcome::Error => Style::new().fg(ERROR_RED),
        ToolOutcome::Refused => yellow,
        ToolOutcome::NotRun => dim,
        ToolOutcome::Running if cancelling => yellow,
        ToolOutcome::Running => Style::new().fg(Color::Cyan),
    }
}

/// The storage state as a word, or None when the row has nothing to add:
/// saved output needs no announcement, because the row already reports the
/// result that survived. The two states that do need one are the cases where
/// the visible text is not all there is and the hidden part is missing.
fn storage_label(storage: ShellStorage) -> Option<&'static str> {
    match storage {
        ShellStorage::Saved => None,
        ShellStorage::SaveFailed => Some("not saved"),
        ShellStorage::Truncated => Some("truncated"),
    }
}

/// The right-block parts beyond the verdict, in the order they survive a
/// limited width: what was kept of the output outranks where it was kept, so
/// the block tag is the first to go. The CAS block address is a storage detail
/// for the /context and /trajectory panes, not the main transcript row.
fn right_extras(row: &ShellRow<'_>) -> Vec<(String, Style)> {
    let warning = Style::new().fg(Color::Yellow);
    let mut extras = Vec::new();
    if let Some(label) = row.storage.and_then(storage_label) {
        extras.push((label.to_string(), warning));
    }
    extras
}

/// Lay out one user shell command on a single row. A command wider than the
/// room is truncated, never the outcome, because the outcome is what the row
/// exists to report; at least one space separates the two so the outcome
/// cannot read as part of the command. Tags drop before the command loses the
/// width it needs to stay readable. Control sequences are removed first: the
/// command can come back from a durable log written by another client.
pub(super) fn shell_command_line(row: &ShellRow<'_>, width: u16) -> (String, Line<'static>) {
    const PROMPT: &str = "$ ";
    const GLUE: &str = " \u{00b7} ";
    /// Command cells a tag may not take. Below this the command cannot be
    /// told apart, so a tag degrades instead of crowding it out.
    const MIN_COMMAND: usize = 8;
    let cyan = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let glue_w = UnicodeWidthStr::width(GLUE);
    let max = width as usize;
    let prompt_w = UnicodeWidthStr::width(PROMPT);
    let status = row.status_text();
    let status = truncate_width(&status, max.saturating_sub(prompt_w + 1));
    let status_w = UnicodeWidthStr::width(status.as_str());
    let mut extras = right_extras(row);
    let extras_w = |extras: &[(String, Style)]| -> usize {
        extras
            .iter()
            .map(|(text, _)| glue_w + UnicodeWidthStr::width(text.as_str()))
            .sum()
    };
    while prompt_w + status_w + extras_w(&extras) + MIN_COMMAND > max {
        if extras.pop().is_none() {
            break;
        }
    }
    let extras_w = extras_w(&extras);
    let right_w = status_w + extras_w;
    let command = strip_ansi(row.command)
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    let room = max.saturating_sub(prompt_w + right_w + 2);
    let command = truncate_width(&command, room);
    let command_w = UnicodeWidthStr::width(command.as_str());
    let gap = max.saturating_sub(prompt_w + command_w + right_w).max(1);
    let mut text = format!("{PROMPT}{command}{}{status}", " ".repeat(gap));
    let mut spans = vec![
        Span::styled(PROMPT, cyan),
        Span::raw(command),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            status,
            status_style(row.outcome, row.timed_out, row.cancelling),
        ),
    ];
    for (part, style) in extras {
        text.push_str(GLUE);
        spans.push(Span::raw(GLUE));
        text.push_str(&part);
        spans.push(Span::styled(part, style));
    }
    (text, Line::from(spans))
}
