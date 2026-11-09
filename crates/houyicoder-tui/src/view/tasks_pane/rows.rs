//! The /tasks pane's row order.
//!
//! One list, in screen order, that both the renderer and the key handlers
//! resolve through, so the row the user sees highlighted is the task an
//! action reaches. Section headings are derived from the row's own section,
//! so a heading can never label a row it does not precede.

use std::cmp::Reverse;

use houyicoder_protocol::frontend::tasks::{BackgroundTaskId, BackgroundTaskSnapshot};

/// One selectable row. A section heading is not a row: it carries no action,
/// so the cursor never lands on one.
pub(crate) enum TaskRow<'a> {
    /// A command that has not reported an ending.
    Running(&'a BackgroundTaskSnapshot),
    /// A command that ended: finished, failed, killed, or timed out.
    Ended(&'a BackgroundTaskSnapshot),
}

impl TaskRow<'_> {
    /// The heading this row is listed under.
    pub(crate) fn section(&self) -> &'static str {
        match self {
            Self::Running(_) => "Running",
            Self::Ended(_) => "Ended",
        }
    }

    /// The task this row names.
    pub(crate) fn task(&self) -> &BackgroundTaskSnapshot {
        match self {
            Self::Running(task) | Self::Ended(task) => task,
        }
    }
}

/// The pane's rows in screen order: commands still running, newest first,
/// then commands that ended, newest first.
///
/// A live command outranks every finished one because it is the only kind the
/// pane can still act on, and the newest of each section is what a reader
/// opening the pane is most likely to reach for. Within a section the order
/// never changes while the task lives, so a row moves only when the task
/// crosses into the other section.
///
/// A status word this build does not name is listed under Running: the pane
/// must not read an unknown word as an ending, and the row itself still shows
/// the word it received.
pub(crate) fn task_rows<'a>(
    tasks: impl Iterator<Item = &'a BackgroundTaskSnapshot>,
) -> Vec<TaskRow<'a>> {
    let mut rows = Vec::new();
    let mut ended = Vec::new();
    for task in tasks {
        if task.status.is_terminal() {
            ended.push(TaskRow::Ended(task));
        } else {
            rows.push(TaskRow::Running(task));
        }
    }
    rows.sort_by_key(|row| Reverse(row.task().id));
    ended.sort_by_key(|row| Reverse(row.task().id));
    rows.extend(ended);
    rows
}

/// The task numbers the rows name, in screen order, so a cursor can be
/// resolved against the same list the user reads.
pub(crate) fn task_ids(rows: &[TaskRow<'_>]) -> Vec<BackgroundTaskId> {
    rows.iter().map(|row| row.task().id).collect()
}

#[cfg(test)]
mod tests;
