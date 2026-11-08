//! The dollar prompt row: a command the user typed, with the outcome right
//! aligned so a reader compares verdicts down one column.

use houyicoder_protocol::envelope::RequestId;
use ratatui::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::composition;
use crate::records::{ToolOutcome, TranscriptLine};
use crate::state::App;
use crate::state::Screen;
use crate::test_harness::{render_buffer, render_text};
use crate::transcript::shell_result::ShellStorage;

fn shell_line(
    command: &str,
    outcome: ToolOutcome,
    duration_ms: Option<u64>,
    exit_code: Option<i64>,
) -> TranscriptLine {
    TranscriptLine::ShellCommand {
        command: command.to_string(),
        outcome,
        duration_ms,
        exit_code,
        timed_out: false,
        storage: None,
        cas: None,
    }
}

fn shell_line_with(
    command: &str,
    storage: Option<ShellStorage>,
    cas: Option<&str>,
) -> TranscriptLine {
    TranscriptLine::ShellCommand {
        command: command.to_string(),
        outcome: ToolOutcome::Success,
        duration_ms: Some(10),
        exit_code: Some(0),
        timed_out: false,
        storage,
        cas: cas.map(str::to_string),
    }
}

fn render_row_in(mut app: App, line: TranscriptLine) -> (String, String) {
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![line].into();
    let out = render_text(&app, 80, 24);
    let row = out
        .lines()
        .find(|row| row.starts_with("$ "))
        .unwrap_or_else(|| panic!("the command row renders:\n{out}"))
        .trim_end()
        .to_string();
    (out, row)
}

fn render_row(line: TranscriptLine) -> (String, String) {
    render_row_in(composition::app(), line)
}

/// A finished command reads as its own prompt line: the command on the left
/// and the verdict, exit code, and elapsed time on the right. The outcome is
/// the part a user scans for, so a long command truncates before it.
#[test]
fn test_shell_row_right_aligned() {
    let (_out, row) = render_row(shell_line(
        "cargo test",
        ToolOutcome::Success,
        Some(1250),
        Some(0),
    ));
    assert!(row.starts_with("$ cargo test"), "command leads: {row:?}");
    assert!(
        row.ends_with("exit 0 · 1.2s"),
        "outcome, code, and time close the row: {row:?}"
    );
    assert_eq!(
        UnicodeWidthStr::width(row.as_str()),
        80,
        "the outcome sits on the row's right edge: {row:?}"
    );
}

/// A command written in wide glyphs lays out on the same grid: the outcome
/// still lands on the right edge, so a column of rows stays aligned wherever
/// the commands come from. The row is read from the buffer because a text dump
/// spends a cell on each half of a wide glyph.
#[test]
fn test_shell_row_wide_glyph() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![shell_line(
        "回声测试",
        ToolOutcome::Success,
        Some(90),
        Some(0),
    )]
    .into();
    let buf = render_buffer(&app, 80, 24);
    let prompt_row = (0..24)
        .find(|y| buf.cell((0, *y)).is_some_and(|cell| cell.symbol() == "$"))
        .unwrap_or_else(|| panic!("the prompt row renders"));
    let last = (0..80)
        .rev()
        .find(|x| {
            buf.cell((*x, prompt_row))
                .is_some_and(|cell| !cell.symbol().trim().is_empty())
        })
        .expect("the row has content");
    assert_eq!(last, 79, "the outcome reaches the right edge");
}

/// A command wider than the row truncates before the outcome, never after:
/// the verdict is what the row exists to report, so it survives any length.
#[test]
fn test_shell_row_long_command() {
    let (_out, row) = render_row(shell_line(
        &"x".repeat(200),
        ToolOutcome::Success,
        Some(10),
        Some(0),
    ));
    assert_eq!(
        UnicodeWidthStr::width(row.as_str()),
        80,
        "a long command still fits one row: {row:?}"
    );
    assert!(row.contains('\u{2026}'), "the command is cut: {row:?}");
    assert!(row.ends_with("exit 0 · 10ms"), "{row:?}");
}

/// A command that starts cleanly and fails still reports its nonzero exit
/// code; the verdict is the code, not a generic failure word.
#[test]
fn test_shell_row_failure_exit() {
    let (_out, row) = render_row(shell_line(
        "cargo build",
        ToolOutcome::Error,
        Some(240),
        Some(101),
    ));
    assert!(row.ends_with("exit 101 · 240ms"), "{row:?}");
}

/// A command still running has no exit code and no elapsed time to report:
/// the row says it is running rather than claiming a verdict it does not
/// have yet.
#[test]
fn test_shell_row_running() {
    let (_out, row) = render_row(shell_line("sleep 30", ToolOutcome::Running, None, None));
    assert!(row.ends_with("running"), "{row:?}");
}

/// The prompt itself is cyan and bold, the same marker the input box uses in
/// direct-shell mode, so a command row is recognizable before it is read.
#[test]
fn test_shell_row_prompt_cyan() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![shell_line(
        "cargo test",
        ToolOutcome::Success,
        Some(10),
        Some(0),
    )]
    .into();
    let buf = render_buffer(&app, 80, 24);
    let dollar = buf
        .content()
        .iter()
        .find(|cell| cell.symbol() == "$")
        .expect("the prompt glyph renders");
    assert_eq!(dollar.style().fg, Some(Color::Cyan), "prompt is cyan");
}

/// The row count path agrees with the draw path for a command row: one row,
/// whichever caller asks. The command is long and holds a newline, so a
/// count that wrapped the flat text or split it on the newline would report
/// more rows than the draw path renders.
#[test]
fn test_shell_row_counts_one() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![shell_line(
        &format!("echo {}\necho tail", "x".repeat(200)),
        ToolOutcome::Success,
        Some(10),
        Some(0),
    )]
    .into();
    let _out = render_text(&app, 80, 24);
    assert_eq!(
        app.fold_aware_rows(None),
        app.whole_scroll().total.get(),
        "count path and draw path agree"
    );
    assert_eq!(
        app.fold_aware_rows(None),
        1,
        "a command row is one row however long its text"
    );
}

/// The color of the last non-space cell on the prompt row, which is the
/// verdict's last glyph whenever the verdict closes the row.
fn verdict_color(app: &App) -> Color {
    let buf = render_buffer(app, 80, 24);
    let y = (0..24)
        .find(|y| buf.cell((0, *y)).is_some_and(|cell| cell.symbol() == "$"))
        .unwrap_or_else(|| panic!("the prompt row renders"));
    let x = (0..80)
        .rev()
        .find(|x| {
            buf.cell((*x, y))
                .is_some_and(|cell| !cell.symbol().trim().is_empty())
        })
        .expect("the row has content");
    buf.cell((x, y))
        .and_then(|cell| cell.style().fg)
        .expect("the verdict is colored")
}

/// The color of a run of text on the command row. The run is located by the
/// row's characters rather than by columns, so a wide glyph earlier in the row
/// cannot shift the lookup.
fn text_color(app: &App, needle: &str) -> Color {
    let buf = render_buffer(app, 80, 24);
    let y = (0..24)
        .find(|y| buf.cell((0, *y)).is_some_and(|cell| cell.symbol() == "$"))
        .unwrap_or_else(|| panic!("the prompt row renders"));
    let mut row = String::new();
    let mut colors = Vec::new();
    for x in 0..80 {
        let Some(cell) = buf.cell((x, y)) else { break };
        for glyph in cell.symbol().chars() {
            row.push(glyph);
            colors.push(cell.style().fg);
        }
    }
    let at = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} renders on the row: {row:?}"));
    let index = row[..at].chars().count();
    colors[index].unwrap_or_else(|| panic!("{needle:?} is colored: {row:?}"))
}

/// A command that hit its time limit reads yellow, not red: the limit is a
/// state the user can act on, not a failure the command reported, and a
/// timed-out call has no exit code to show.
#[test]
fn test_shell_row_timeout_yellow() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    let line = TranscriptLine::ShellCommand {
        command: "sleep 60".into(),
        outcome: ToolOutcome::Error,
        duration_ms: Some(1500),
        exit_code: None,
        timed_out: true,
        storage: None,
        cas: None,
    };
    app.sessions.current.transcript = vec![line].into();
    let out = render_text(&app, 80, 24);
    let row = out
        .lines()
        .find(|row| row.starts_with("$ "))
        .expect("the command row renders")
        .trim_end();
    assert!(row.ends_with("timed out · 1.5s"), "{row:?}");
    assert_eq!(verdict_color(&app), Color::Yellow, "the limit reads yellow");
}

/// While the user's stop is on its way, the row says so at once instead of
/// still claiming the command runs: the user acted and needs the row to
/// acknowledge it before the result lands.
#[test]
fn test_shell_row_stopping_yellow() {
    let mut app = composition::app();
    app.sessions.current.shell.begin(RequestId(7));
    app.sessions.current.shell.begin_cancel();
    assert!(app.sessions.current.shell.is_cancelling());
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript =
        vec![shell_line("sleep 30", ToolOutcome::Running, None, None)].into();
    let out = render_text(&app, 80, 24);
    let row = out
        .lines()
        .find(|row| row.starts_with("$ "))
        .expect("the command row renders")
        .trim_end();
    assert!(row.ends_with("stopping"), "{row:?}");
    assert_eq!(verdict_color(&app), Color::Yellow, "stopping reads yellow");
}

/// A command the user refused says so on the row and reads yellow: the
/// refusal is a decision the user can still revisit, not a failure the
/// command reported on its own.
#[test]
fn test_shell_row_refused_yellow() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![shell_line(
        "rm -rf build",
        ToolOutcome::Refused,
        Some(0),
        None,
    )]
    .into();
    let out = render_text(&app, 80, 24);
    let row = find_shell_row(&out);
    assert!(row.ends_with("refused · 0ms"), "{row:?}");
    assert_eq!(verdict_color(&app), Color::Yellow, "a refusal reads yellow");
}

/// The block-store address is a storage detail for the /context and
/// /trajectory panes, not the main transcript row. A shell result that moved
/// its output to the block store still reports the storage word (truncated,
/// saved), but the CAS hash never reaches the row a user reads.
#[test]
fn test_shell_row_omits_cas() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    let line = TranscriptLine::ShellCommand {
        command: "cargo test".into(),
        outcome: ToolOutcome::Success,
        duration_ms: Some(10),
        exit_code: Some(0),
        timed_out: false,
        storage: None,
        cas: Some("CAS a1b2c3d4 (22.5 KiB)".into()),
    };
    app.sessions.current.transcript = vec![line].into();
    let out = render_text(&app, 80, 24);
    let row = out
        .lines()
        .find(|row| row.starts_with("$ "))
        .expect("the command row renders")
        .trim_end();
    assert!(
        !row.contains("CAS") && !row.contains("a1b2c3d4"),
        "the block address stays out of the main row: {row:?}"
    );
    assert!(
        row.ends_with("exit 0 · 10ms"),
        "the verdict and time still report: {row:?}"
    );
}

fn find_shell_row(out: &str) -> String {
    out.lines()
        .find(|row| row.starts_with("$ "))
        .unwrap_or_else(|| panic!("the command row renders:\n{out}"))
        .trim_end()
        .to_string()
}

/// A command that exited cleanly but whose output could not be kept does not
/// read as a plain success: the verdict stays green because the command did
/// succeed, and a yellow "not saved" says the raw output above the visible
/// text is gone.
#[test]
fn test_shell_row_failed_save() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![shell_line_with(
        "cargo test",
        Some(ShellStorage::SaveFailed),
        None,
    )]
    .into();
    let out = render_text(&app, 80, 24);
    let row = find_shell_row(&out);
    assert!(row.ends_with("exit 0 · 10ms · not saved"), "{row:?}");
    assert_eq!(
        text_color(&app, "exit 0"),
        Color::Rgb(78, 186, 101),
        "the verdict keeps the success color: the command did exit cleanly"
    );
    assert_eq!(
        text_color(&app, "not saved"),
        Color::Yellow,
        "the loss reads yellow"
    );
}

/// A capture that stopped the command says so on the row, so a reader does
/// not take the visible output for everything the command produced.
#[test]
fn test_shell_row_truncated() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![shell_line_with(
        "cargo test",
        Some(ShellStorage::Truncated),
        None,
    )]
    .into();
    let out = render_text(&app, 80, 24);
    let row = find_shell_row(&out);
    assert!(row.ends_with("exit 0 · 10ms · truncated"), "{row:?}");
}

/// The storage word fits a limited width while the block address never
/// reaches the row at all. At 50 columns the verdict and the storage word
/// fit with a readable command; the CAS hash is a pane detail and stays out
/// of the main row regardless of width, so the room is measured only over the
/// tags the row actually shows.
#[test]
fn test_shell_row_tag_degrades() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![shell_line_with(
        "cargo test",
        Some(ShellStorage::Truncated),
        Some("CAS a1b2c3d4 (22.5 KiB)"),
    )]
    .into();
    let out = render_text(&app, 50, 24);
    let row = find_shell_row(&out);
    assert!(row.ends_with("truncated"), "{row:?}");
    assert!(!row.contains("CAS"), "the block tag gives way: {row:?}");
    assert!(
        UnicodeWidthStr::width(row.as_str()) <= 50,
        "the row stays inside the terminal: {row:?}"
    );
}

/// A command written across several lines renders on one row: the row is a
/// single summary line, so a multi-line command cannot break the verdict
/// column.
#[test]
fn test_shell_row_single_line() {
    let (_out, row) = render_row(shell_line(
        "echo one\necho two",
        ToolOutcome::Success,
        Some(10),
        Some(0),
    ));
    assert!(row.starts_with("$ echo one echo two"), "{row:?}");
}

/// A command read back from a durable log may carry terminal control
/// sequences. The row removes them before laying it out, so they cannot
/// repaint the terminal or take the columns the verdict needs.
#[test]
fn test_shell_row_controls_removed() {
    let (_out, row) = render_row(shell_line(
        "echo \u{1b}[31mred\u{1b}[0m",
        ToolOutcome::Success,
        Some(10),
        Some(0),
    ));
    assert!(
        !row.contains('\u{1b}'),
        "control sequences are removed: {row:?}"
    );
    assert!(row.starts_with("$ echo red"), "{row:?}");
    assert_eq!(
        UnicodeWidthStr::width(row.as_str()),
        80,
        "the verdict still reaches the right edge: {row:?}"
    );
}
