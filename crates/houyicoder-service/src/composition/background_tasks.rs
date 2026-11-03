//! The host's record of commands that outlived their tool call.
//!
//! A command that crosses the foreground budget moves here. The
//! registry assigns its number, keeps its latest state for the tasks list and
//! the detail view, and owns the one cancellation token that ends it. When the
//! command ends the registry publishes a final snapshot for the frontend and
//! reports the ending to the model through the runner's notification queue,
//! which delivers it at the next turn boundary.

use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use houyicoder_api::background::{
    BackgroundCommandLauncher, CommandCompletion, CommandTail, DetachedCommand,
};
use houyicoder_async::CancellationToken;
use houyicoder_async::PFut;
use houyicoder_context::NotificationTopic;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::frontend::FrontendEvent;
use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskDetail, BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};
use houyicoder_protocol::tool::bash_exit_succeeded;
use serde_json::Value;

use crate::server::EventSequencer;

/// Finished commands kept for the tasks list. A session that runs many
/// commands keeps its newest endings; older ones are released so the list
/// stays a window over recent work rather than a log that grows without end.
const RETAINED_FINISHED: usize = 50;

/// Characters kept from the command's first line, so a long one-line command
/// does not push its own ending out of the row that shows it.
const COMMAND_LINE_CHARS: usize = 200;

/// One command the registry owns.
struct TaskRecord {
    id: BackgroundTaskId,
    /// The first line of the command, bounded.
    command: String,
    started: Instant,
    /// Frozen when the command ended; None while it runs.
    ended_at: Option<Instant>,
    status: BackgroundTaskStatus,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    /// The live preview window the running command keeps writing into.
    tail: CommandTail,
    cancel: CancellationToken,
}

struct TaskTable {
    next_id: u64,
    /// Oldest first, so pruning the oldest finished records walks from the
    /// front and the list reads in assignment order.
    tasks: Vec<TaskRecord>,
}

impl Default for TaskTable {
    fn default() -> Self {
        Self {
            // Numbering starts at one: the number is what a row shows the
            // user, and the first task reads as task 1, not task 0.
            next_id: 1,
            tasks: Vec::new(),
        }
    }
}

/// The part of the registry a finishing command still needs after the
/// registry itself may have been released.
struct Inner {
    table: Mutex<TaskTable>,
    /// Canceled when the registry drops, which ends every command still
    /// running. Each task holds a child token of this one.
    shutdown: CancellationToken,
}

/// One command's ending: the completion the run produced, or the
/// cancellation that stopped it before it produced one.
enum Ending {
    Completed(CommandCompletion),
    Killed,
}

/// What a finished command reports: how it ended, what it printed, and the
/// stored output address it published. The list, the message to the model,
/// and the notification read one report rather than each rebuilding the
/// same fields from the ending.
struct EndingReport {
    status: BackgroundTaskStatus,
    exit_code: Option<i32>,
    error: Option<String>,
    stdout: String,
    stderr: String,
    capture: Option<Value>,
}

impl EndingReport {
    fn of(command: &str, ending: Ending) -> Self {
        match ending {
            Ending::Killed => Self {
                status: BackgroundTaskStatus::Killed,
                exit_code: None,
                error: None,
                stdout: String::new(),
                stderr: String::new(),
                capture: None,
            },
            Ending::Completed(completion) => Self {
                status: completion_status(command, &completion),
                exit_code: completion.exit_code,
                error: completion.error,
                stdout: completion.stdout,
                stderr: completion.stderr,
                capture: completion.capture,
            },
        }
    }
}

/// The session's background command registry.
///
/// The tools pass a run through the launcher port; the frontend reads the
/// list, the detail, and kills through the protocol. The registry is the only
/// owner of a task's number and state, so both readers see one answer.
pub struct BackgroundTasks {
    inner: Arc<Inner>,
    /// Where run state is published once the server exists. A registry
    /// nobody attached still runs commands and answers the list; it just has
    /// nowhere to report.
    sequencer: Mutex<Option<EventSequencer>>,
    /// The session's runner, held weakly so a finishing command never keeps a
    /// released session alive.
    runner: Mutex<Weak<Runner>>,
}

impl Default for BackgroundTasks {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTasks {
    /// A registry owning no commands yet.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                table: Mutex::new(TaskTable::default()),
                shutdown: CancellationToken::new(),
            }),
            sequencer: Mutex::new(None),
            runner: Mutex::new(Weak::new()),
        }
    }

    /// Point the registry at the session's runner, and at the event stream
    /// when the session has a frontend to publish into.
    ///
    /// Both arrive after the tool provider was built: the sequencer is
    /// created with the server, and the runner only becomes shareable once
    /// its Arc exists. A session without a frontend passes no sequencer; a
    /// task's ending then reaches the model through the runner alone.
    pub fn attach(&self, sequencer: Option<EventSequencer>, runner: &Arc<Runner>) {
        *self.sequencer.lock().expect("sequencer slot") = sequencer;
        *self.runner.lock().expect("runner slot") = Arc::downgrade(runner);
    }

    /// Every task the registry holds, oldest first.
    pub fn list(&self) -> Vec<BackgroundTaskSnapshot> {
        let table = self.inner.table.lock().expect("background tasks lock");
        table.tasks.iter().map(snapshot_of).collect()
    }

    /// One task's full record at the moment of the read, with the tail of a
    /// running command as it stands now.
    pub fn detail(&self, id: BackgroundTaskId) -> Option<BackgroundTaskDetail> {
        let table = self.inner.table.lock().expect("background tasks lock");
        table
            .tasks
            .iter()
            .find(|task| task.id == id)
            .map(|record| BackgroundTaskDetail {
                id: record.id,
                status: record.status.clone(),
                command: record.command.clone(),
                elapsed_ms: elapsed_ms(record),
                exit_code: record.exit_code,
                stdout: record.stdout.clone(),
                stderr: record.stderr.clone(),
                tail: record.tail.snapshot(),
            })
    }

    /// End every running command now.
    ///
    /// The session's teardown calls this rather than waiting for the last
    /// handle to drop: the registry is shared by the host, the server, and the
    /// runner, so a session that ends while one of them still holds a handle
    /// would leave its commands running with nobody left to read them.
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }

    /// Stop one running command. Returns false when the number names no task
    /// or the task already ended, so a caller can tell a kill that took effect
    /// from one that found nothing to stop.
    pub fn kill(&self, id: BackgroundTaskId) -> bool {
        let token = {
            let table = self.inner.table.lock().expect("background tasks lock");
            match table.tasks.iter().find(|task| task.id == id) {
                Some(task) if !task.status.is_terminal() => Some(task.cancel.clone()),
                _ => None,
            }
        };
        match token {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Publish one task's current state to the frontend.
    fn publish(&self, id: BackgroundTaskId) {
        let sequencer = self.sequencer.lock().expect("sequencer slot").clone();
        let Some(sequencer) = sequencer else {
            return;
        };
        let snapshot = {
            let table = self.inner.table.lock().expect("background tasks lock");
            table
                .tasks
                .iter()
                .find(|task| task.id == id)
                .map(snapshot_of)
        };
        if let Some(task) = snapshot {
            sequencer.enqueue_reliable(FrontendEvent::BackgroundTaskChanged { task });
        }
    }
}

impl Drop for BackgroundTasks {
    /// End every running command with the session. The completion future is
    /// dropped by each run's cancellation branch, which ends the command's
    /// process group through the same guard a foreground cancel uses.
    fn drop(&mut self) {
        self.inner.shutdown.cancel();
    }
}

impl BackgroundCommandLauncher for BackgroundTasks {
    fn launch(&self, command: DetachedCommand) -> BackgroundTaskId {
        let DetachedCommand {
            command,
            started_at,
            tail,
            completion,
        } = command;
        let (id, cancel) = {
            let mut table = self.inner.table.lock().expect("background tasks lock");
            let id = BackgroundTaskId(table.next_id);
            table.next_id = table.next_id.saturating_add(1);
            let cancel = self.inner.shutdown.child_token();
            table.tasks.push(TaskRecord {
                id,
                command: first_line(&command),
                started: started_at,
                ended_at: None,
                status: BackgroundTaskStatus::Running,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                tail: tail.clone(),
                cancel: cancel.clone(),
            });
            (id, cancel)
        };
        self.publish(id);
        tracing::debug!(task = id.0, command = %first_line(&command), "background task started");
        // The tool that passes a run over runs on the host runtime, so the
        // finishing driver joins that same runtime and outlives the tool call.
        tokio::spawn(finish_run(
            Arc::clone(&self.inner),
            self.sequencer.lock().expect("sequencer slot").clone(),
            self.runner.lock().expect("runner slot").clone(),
            id,
            cancel,
            completion,
            command,
            tail,
        ));
        id
    }
}

/// Wait for one background run to end, record how it ended, then publish the
/// final state and tell the model.
///
/// The completion is offered first: a command that already ended has an
/// answer, and a cancel that arrived in the same instant must not discard the
/// output it produced. A cancel that arrives while the command still runs
/// drops the completion future, which ends the command; the run then reports
/// Killed without waiting for a process that no longer has a reader.
#[expect(
    clippy::too_many_arguments,
    reason = "the inputs are the run as frozen at launch"
)]
async fn finish_run(
    inner: Arc<Inner>,
    sequencer: Option<EventSequencer>,
    runner: Weak<Runner>,
    id: BackgroundTaskId,
    cancel: CancellationToken,
    completion: PFut<'static, CommandCompletion>,
    command: String,
    tail: CommandTail,
) {
    let outcome = tokio::select! {
        biased;
        completion = completion => Ending::Completed(completion),
        _ = cancel.cancelled() => Ending::Killed,
    };
    let report = EndingReport::of(&command, outcome);
    let status = report.status.clone();
    let exit_code = report.exit_code;
    tracing::debug!(
        task = id.0,
        status = %status.as_str(),
        exit_code = ?exit_code,
        "background task ended"
    );
    let text = notification_text(id, &first_line(&command), &report, &tail.snapshot());
    let snapshot = {
        let mut table = inner.table.lock().expect("background tasks lock");
        let snapshot = table
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .map(|record| {
                record.status = status;
                record.ended_at = Some(Instant::now());
                record.exit_code = exit_code;
                record.stdout = report.stdout;
                record.stderr = report.stderr;
                snapshot_of(record)
            });
        prune_finished(&mut table);
        snapshot
    };
    if let (Some(sequencer), Some(task)) = (sequencer, snapshot) {
        sequencer.enqueue_reliable(FrontendEvent::BackgroundTaskChanged { task });
    }
    let Some(runner) = runner.upgrade() else {
        // The session that would read the ending is gone, so the notification
        // has no reader; the diagnostic log is the only place left to name it.
        tracing::debug!(task = id.0, "background task ended after its session");
        return;
    };
    runner.enqueue_notification(
        task_source_id(id),
        NotificationTopic::Task,
        text,
        report.capture,
    );
}

/// The state a finished run reports. A run that never produced a normal
/// result is a failure whatever its exit status; a run that reached its own
/// wall-clock budget is a timeout; otherwise the exit status decides, read by
/// the same rule a tool result uses, so a search that finds nothing is not
/// reported as a failed task.
fn completion_status(command: &str, completion: &CommandCompletion) -> BackgroundTaskStatus {
    if completion.error.is_some() {
        BackgroundTaskStatus::Failed
    } else if completion.timed_out {
        BackgroundTaskStatus::TimedOut
    } else if completion
        .exit_code
        .is_some_and(|code| bash_exit_succeeded(command, i64::from(code)))
    {
        BackgroundTaskStatus::Finished
    } else {
        BackgroundTaskStatus::Failed
    }
}

/// Drop the oldest finished records once the retained count is over its cap.
/// Running records are never dropped, so a list under pressure loses history
/// rather than a task the user can still see working.
fn prune_finished(table: &mut TaskTable) {
    let mut excess = table
        .tasks
        .iter()
        .filter(|task| task.status.is_terminal())
        .count()
        .saturating_sub(RETAINED_FINISHED);
    if excess == 0 {
        return;
    }
    table.tasks.retain(|task| {
        if excess > 0 && task.status.is_terminal() {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

/// The text the model reads at the next turn boundary: which task, how it
/// ended, what it ran, and the output it printed. The output is the bounded
/// copy the foreground answer would have carried, so a command that ends
/// after the background move still gives the model something to act on.
fn notification_text(
    id: BackgroundTaskId,
    command: &str,
    report: &EndingReport,
    tail: &[String],
) -> String {
    let mut text = format!(
        "<task-notification id=\"task-{}\" status=\"{}\"",
        id.0,
        report.status.as_str()
    );
    if let Some(code) = report.exit_code {
        text.push_str(&format!(" exit_code=\"{code}\""));
    }
    text.push_str(">\ncommand:");
    push_quoted(&mut text, command);
    if let Some(error) = report.error.as_deref() {
        text.push_str("\nerror:");
        push_quoted(&mut text, error);
    }
    if !report.stdout.is_empty() {
        text.push_str("\nstdout:");
        push_quoted(&mut text, &report.stdout);
    }
    if !report.stderr.is_empty() {
        text.push_str("\nstderr:");
        push_quoted(&mut text, &report.stderr);
    }
    // A killed command produced no bounded answer, so the live window is the
    // only thing it printed; a finished one already carries its output above.
    if report.stdout.is_empty() && report.stderr.is_empty() && !tail.is_empty() {
        text.push_str("\nlast output:");
        push_quoted(&mut text, &tail.join("\n"));
    }
    text.push_str("\n</task-notification>");
    text
}

/// Add a body as quoted lines under the field that names it.
///
/// Every line carries the marker, so no line the command printed can stand
/// alone as part of the block: a line equal to the closing tag would end the
/// block early, and whatever followed it would read as a statement in its
/// own right rather than as output.
fn push_quoted(text: &mut String, body: &str) {
    for line in body.lines() {
        text.push('\n');
        text.push_str("| ");
        text.push_str(line);
    }
}

/// The id the durable notification records for this task.
fn task_source_id(id: BackgroundTaskId) -> String {
    format!("task-{}", id.0)
}

fn snapshot_of(record: &TaskRecord) -> BackgroundTaskSnapshot {
    BackgroundTaskSnapshot {
        id: record.id,
        status: record.status.clone(),
        command: record.command.clone(),
        elapsed_ms: elapsed_ms(record),
        exit_code: record.exit_code,
    }
}

/// Whole milliseconds the command has run: frozen once it ended, counted up
/// to now while it runs.
fn elapsed_ms(record: &TaskRecord) -> u64 {
    let end = record.ended_at.unwrap_or_else(Instant::now);
    u64::try_from(end.saturating_duration_since(record.started).as_millis()).unwrap_or(u64::MAX)
}

/// The command's first line, bounded, as the task row shows it.
fn first_line(command: &str) -> String {
    let line = command.lines().next().unwrap_or_default();
    line.chars().take(COMMAND_LINE_CHARS).collect()
}

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
