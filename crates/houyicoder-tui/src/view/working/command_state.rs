//! The rows a running or background command shows below its call chip: the
//! live elapsed time and the call's own limit with a preview of what it is
//! printing, and the ending a background command reported. Both the draw path
//! and the row-count path read these builders, so the two always agree on how
//! many rows one call occupies.

use houyicoder_protocol::frontend::tasks::{BackgroundTaskSnapshot, BackgroundTaskStatus};
use houyicoder_protocol::tool::strip_ansi;

use crate::records::TranscriptLine;
use crate::state::App;
use crate::view::line_wrap::truncate_width;
use crate::view::relative_time::format_span_secs;
use crate::view::relative_time::summary_span_label;

/// Seconds a call runs before its row starts reporting: a shorter one is over
/// before a reader could look, and a row that appears and vanishes is noise.
const PROGRESS_AFTER_SECS: u64 = 2;

/// Output lines the running row previews. Enough to see that a long build is
/// moving, few enough that the preview does not take the transcript over.
const PREVIEW_LINES: usize = 5;

/// The rows under a call chip that is still running in the foreground: the
/// elapsed time with the call's own limit, then the tail of its output. Empty
/// for any line that is not a running chip, and for a call younger than the
/// reporting threshold. A background call's result row carries the state
/// instead, so once the chip has a result the running rows are gone.
pub(crate) fn running_rows(app: &App, line: &TranscriptLine, width: u16) -> Vec<String> {
    let TranscriptLine::Tool {
        name,
        call_id,
        background_task: None,
        ..
    } = line
    else {
        return Vec::new();
    };
    if name == "result" {
        return Vec::new();
    }
    let Some(progress) = app.run_progress().and_then(|p| p.tools.tick(call_id)) else {
        return Vec::new();
    };
    if progress.elapsed_secs < PROGRESS_AFTER_SECS {
        return Vec::new();
    }
    let elapsed = format_span_secs(progress.elapsed_secs);
    let head = match progress.timeout_secs {
        Some(limit) => format!(
            "  \u{23bf}  Running\u{2026} ({elapsed} \u{00b7} timeout {})",
            format_span_secs(limit)
        ),
        None => format!("  \u{23bf}  Running\u{2026} ({elapsed})"),
    };
    let mut rows = vec![truncate_width(&head, width as usize)];
    let tail = progress
        .tail
        .iter()
        .map(|line| strip_ansi(line))
        .filter(|line| !line.trim().is_empty());
    let preview: Vec<String> = tail.collect();
    let first = preview.len().saturating_sub(PREVIEW_LINES);
    for line in &preview[first..] {
        let row = format!("     {}", line.trim_end());
        rows.push(truncate_width(&row, width as usize));
    }
    rows
}

/// The body a background call's result row shows now that its task has
/// ended, or None while the task runs (the built row already names it) and
/// for any line that is not such a result. The row keeps the ending, the
/// measured run time, and the exit status the host reported. A task the
/// listing no longer holds was ended and released by the host, so the row
/// says that instead of claiming the command is still running.
pub(crate) fn ended_body(app: &App, line: &TranscriptLine) -> Option<String> {
    let TranscriptLine::Tool {
        name,
        background_task: Some(id),
        ..
    } = line
    else {
        return None;
    };
    if name != "result" {
        return None;
    }
    let tasks = &app.sessions.current.capabilities.tasks;
    let Some(task) = tasks.get(*id) else {
        return tasks
            .listed()
            .then(|| format!("task {} is no longer tracked", id.label()));
    };
    if !task.status.is_terminal() {
        return None;
    }
    Some(ending_text(task))
}

/// How one ended task reads as a row: the ending, the run time it took, and
/// the exit status when the host reported one.
fn ending_text(task: &BackgroundTaskSnapshot) -> String {
    let span = summary_span_label(task.elapsed_ms);
    let exit = task
        .exit_code
        .map(|code| format!(" (exit {code})"))
        .unwrap_or_default();
    match &task.status {
        BackgroundTaskStatus::Finished => format!("Finished in {span}{exit}"),
        BackgroundTaskStatus::Failed => format!("Failed after {span}{exit}"),
        BackgroundTaskStatus::Killed => format!("Killed after {span}"),
        BackgroundTaskStatus::TimedOut => format!("Timed out after {span}"),
        _ => format!("Ended after {span}{exit}"),
    }
}
