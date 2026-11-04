//! The durable records that close a delegation boundary: the return a child's
//! terminal state leaves in the parent log, and the record the model reads
//! when an earlier host never wrote that return.

use houyicoder_api::session::{
    SessionLog, aggregate_subagent_usage, last_clear_index, recorded_turn_usage,
};
use houyicoder_context::{NotificationTopic, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::tool::{failure_rule_input, retained_command, tool_result_failed};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

use crate::agent::append::new_event;
use crate::agent::tools::orphaned_result;

use super::SpawnError;

/// The child's own tool calls the log records. A child this process ran
/// reports these from its resident summary; a child whose host restarted
/// never entered this process's summary, so the scan that reconciles it folds
/// them from the child's log instead of reporting zeroes for work the log
/// records. A child that itself delegated counts only its own calls here —
/// its grandchildren's figures ride its usage, not its ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChildLedger {
    /// Tool calls the child issued in its current epoch.
    pub tool_calls: u32,
    /// Tool calls whose result reports a failure.
    pub tool_failures: u32,
    /// The span from the epoch's first recorded event to its last.
    pub duration_ms: u64,
}

impl ChildLedger {
    /// Fold a child's replayed log into the figures its return reports.
    ///
    /// The fold starts at the newest clear marker when the log records one, the
    /// same window a resident summary describes. A compaction boundary moves
    /// neither window: the summary keeps counting the turns behind it, and
    /// this fold reads them the same way, so a child that compacted reports
    /// the same figures either way.
    ///
    /// The failure test is the one the live summary applies to each result, so
    /// a call a user rejected counts as neither a failure nor a success. Only
    /// the command text that test can read is kept for each pending call, the
    /// same reduction the live fold applies.
    pub fn from_log(log: &[SessionLogEntry]) -> Self {
        let mut ledger = Self::default();
        let mut issued: HashMap<String, (String, Option<String>)> = HashMap::new();
        let window = &log[last_clear_index(log).unwrap_or(0)..];
        for entry in window {
            match &entry.event {
                SessionEvent::ToolCall {
                    call_id,
                    tool,
                    input,
                } => {
                    ledger.tool_calls = ledger.tool_calls.saturating_add(1);
                    let command = input
                        .get("command")
                        .and_then(|c| c.as_str())
                        .and_then(retained_command);
                    issued.insert(call_id.clone(), (tool.clone(), command));
                }
                SessionEvent::ToolResult {
                    call_id, output, ..
                } => {
                    if let Some((tool, command)) = issued.remove(call_id) {
                        let input = failure_rule_input(command.as_deref());
                        if tool_result_failed(output, &tool, &input) {
                            ledger.tool_failures = ledger.tool_failures.saturating_add(1);
                        }
                    }
                }
                _ => {}
            }
        }
        ledger.duration_ms = match (window.first(), window.last()) {
            (Some(first), Some(last)) => last.ts.saturating_sub(first.ts),
            _ => 0,
        };
        ledger
    }
}

/// The token usage a child's own log records for its current epoch: its own
/// provider calls plus the returns of the children it delegated to, each of
/// which already folded its own subtree.
///
/// A row that reports no count keeps its provenance flag clear, so a reader
/// that sums usage still tells an unreported figure from a measured zero. The
/// fold starts at the newest clear marker when the log records one, the window
/// the child's resident summary describes; a compaction boundary moves
/// neither, so a child that compacted reports the same spend either way.
pub fn logged_usage(log: &[SessionLogEntry]) -> Usage {
    let window = &log[last_clear_index(log).unwrap_or(0)..];
    window
        .iter()
        .filter_map(|entry| recorded_turn_usage(&entry.event).map(|(_, usage)| usage))
        .fold(aggregate_subagent_usage(window).to_usage(), |sum, usage| {
            sum.saturating_add(&usage)
        })
}

/// What a child this process ran reports, read from the store's resident
/// summary, or None when the child id does not name a session. A child this
/// process never read reports nothing; the restart scan folds its log
/// instead. None keeps an id this host cannot read from becoming a measured
/// zero in the return.
fn resident_ledger(store: &dyn SessionLog, child: &str) -> Option<ChildLedger> {
    let sid = SessionId::from_display_string(child)?;
    let summary = store.trajectory_head(sid).summary;
    Some(ChildLedger {
        tool_calls: summary.usage.tool_calls as u32,
        tool_failures: summary.usage.failures as u32,
        duration_ms: summary.duration_ms,
    })
}

/// One delegation boundary as it is written to the parent log.
struct ReturnRecord<'a> {
    child_session_id: &'a str,
    status: &'a str,
    summary: &'a str,
    result_ref: &'a str,
    usage: &'a Usage,
    /// The child's measured spend, or None when the writer could not read it.
    ledger: Option<ChildLedger>,
    /// Whether a reconciliation pass wrote this return instead of the host
    /// that ran the child.
    orphaned: bool,
}

/// Append the SubagentReturn boundary to the parent log: the durable marker
/// that closes the spawn a delegation recorded, so replay reconstructs the
/// child's ending with its summary and usage. The runtime writes this after
/// driving a foreground child to terminal.
///
/// The caller holds the runtime's claim on this child's boundary, which is
/// what keeps a live ending and a restart scan from both writing one. The
/// claim is in memory because a boundary is only ever written by the host
/// that owns the child; a restarted host has no claim of its own and reads the
/// log to see which spawns are still open.
pub async fn record_subagent_return(
    store: &dyn SessionLog,
    parent_sid: SessionId,
    child_session_id: &str,
    status: &str,
    summary: &str,
    result_ref: &str,
    usage: &Usage,
) -> Result<(), SpawnError> {
    write_return(
        store,
        parent_sid,
        &ReturnRecord {
            child_session_id,
            status,
            summary,
            result_ref,
            usage,
            ledger: resident_ledger(store, child_session_id),
            orphaned: false,
        },
    )
    .await
}

/// The record a delegation boundary still owes the parent model.
pub enum RecordOwed<'a> {
    /// Answer the model's delegation call with the envelope that names the
    /// child. The call is visible in the model's epoch; whether it still has
    /// no result is decided when the answer is appended.
    Answer {
        /// The call the answer belongs to.
        call_id: &'a str,
    },
    /// Announce the ending through the completion notice, because the
    /// delegation call is outside the model's epoch.
    Notice,
}

/// One delegation boundary as a reconciliation pass found it.
pub struct OrphanedChild<'a> {
    /// The child session whose boundary this record closes.
    pub child_session_id: &'a str,
    /// Whether the parent log still has a spawn for this child that no return
    /// closed. A boundary left that way needs its return; one already closed
    /// needs only the record the model has not read yet.
    pub boundary_open: bool,
    /// The status word the model's record carries: what the pass observed for
    /// a boundary it closes, and the return's own status for one a live
    /// ending closed.
    pub status: &'a str,
    /// The summary the model reads, built by the calling layer.
    pub summary: &'a str,
    /// The record the boundary still owes the model.
    pub record: RecordOwed<'a>,
    /// The usage folded from the child's replayed log, or None when the log
    /// could not be read.
    pub usage: Option<Usage>,
    /// The figures folded from the child's replayed log, or None when the log
    /// could not be read.
    pub ledger: Option<ChildLedger>,
    /// The completion notice the model reads, formatted by the calling layer
    /// through the same function a live completion uses, so both read alike.
    pub notice: &'a str,
}

/// How a pass closed one delegation boundary.
pub enum BoundaryOutcome {
    /// The pass wrote everything the boundary owed.
    Closed,
    /// A live ending settled the boundary first and owns the model's record.
    Settled,
}

/// Close a delegation boundary an earlier host left open: write the missing
/// return when the boundary is still open, then the record the model reads.
///
/// Each step is skipped when the log already holds it, so a pass that wrote
/// the return and then failed before the model's record resumes here rather
/// than deciding from the return that the work was finished. An answer the
/// model already holds ends the pass: the notice is the record for a call the
/// model cannot see, not a second report of one it has read.
///
/// Returns Settled when the log closed the boundary between the scan's read
/// and this write, which means a live ending wrote the return and the model's
/// normal completion path owns the record; the scan writes nothing more.
pub async fn record_orphaned_child(
    store: &dyn SessionLog,
    parent_sid: SessionId,
    found: &OrphanedChild<'_>,
) -> Result<BoundaryOutcome, SpawnError> {
    if found.boundary_open {
        let usage = found.usage.clone().unwrap_or_default();
        let written = settle_open_return(
            store,
            parent_sid,
            &ReturnRecord {
                child_session_id: found.child_session_id,
                status: found.status,
                summary: found.summary,
                result_ref: found.child_session_id,
                usage: &usage,
                ledger: found.ledger,
                orphaned: true,
            },
        )
        .await?;
        if !written {
            return Ok(BoundaryOutcome::Settled);
        }
    }
    match found.record {
        RecordOwed::Answer { call_id } => match append_orphaned_result(
            store,
            parent_sid,
            found.child_session_id,
            call_id,
            found.status,
            found.summary,
        )
        .await?
        {
            AnswerAttempt::Written | AnswerAttempt::AlreadyAnswered => {}
            AnswerAttempt::NotVisible => {
                append_notice(store, parent_sid, found.child_session_id, found.notice).await?;
            }
        },
        RecordOwed::Notice => {
            append_notice(store, parent_sid, found.child_session_id, found.notice).await?;
        }
    }
    Ok(BoundaryOutcome::Closed)
}

/// The summary an orphaned child's return carries: it names the restart and
/// the id that continues the work, so a log reader or the transcript can act
/// without another lookup.
pub fn orphaned_summary(child: &str, partial: Option<&str>) -> String {
    match partial {
        Some(partial) if !partial.is_empty() => {
            format!("[orphaned by host restart, resumable via {child}]\n{partial}")
        }
        _ => format!("[orphaned by host restart, resumable via {child}]"),
    }
}

/// What happened when the pass tried to answer a delegation call.
enum AnswerAttempt {
    /// The answer was appended.
    Written,
    /// The model already holds a result for this call, which is the whole
    /// record the boundary owed.
    AlreadyAnswered,
    /// The call is outside the model's epoch, so an answer would never reach
    /// it and the notice takes its place.
    NotVisible,
}

/// Answer the model's delegation call with the envelope the tool itself
/// returns, when that call still has no result in the view the model reads.
///
/// The append condition runs under the store's append lock, so it is also
/// where the pass learns whether the model already holds the answer or the
/// call has left its epoch.
async fn append_orphaned_result(
    store: &dyn SessionLog,
    parent_sid: SessionId,
    child: &str,
    call_id: &str,
    status: &str,
    summary: &str,
) -> Result<AnswerAttempt, SpawnError> {
    let event = new_event(
        parent_sid,
        SessionEvent::ToolResult {
            call_id: call_id.to_string(),
            output: orphaned_result(child, status, summary),
            duration_ms: 0,
        },
    );
    let call = call_id.to_string();
    let seen_answer = AtomicBool::new(false);
    let unanswered = |log: &[SessionLogEntry]| {
        let visible = &log[last_clear_index(log).unwrap_or(0)..];
        let asked = visible.iter().any(|entry| {
            matches!(&entry.event, SessionEvent::ToolCall { call_id, .. } if call_id == &call)
        });
        let answered = visible.iter().any(|entry| {
            matches!(&entry.event, SessionEvent::ToolResult { call_id, .. } if call_id == &call)
        });
        seen_answer.store(answered, Ordering::Release);
        asked && !answered
    };
    let written = store.append_if(event, &unanswered).await.map_err(|error| {
        warn!(?error, child, "orphaned delegation result append failed");
        SpawnError::BoundaryWriteFailed
    })?;
    Ok(match written {
        Some(_) => AnswerAttempt::Written,
        None if seen_answer.load(Ordering::Acquire) => AnswerAttempt::AlreadyAnswered,
        None => AnswerAttempt::NotVisible,
    })
}

/// Whether the log still owes the model a completion notice for this child.
///
/// A notice counts only when it follows the child's newest spawn, and only
/// inside the epoch the model reads: a resumed child writes a second spawn,
/// and the notice the earlier ending produced reports work that run already
/// finished. The newest spawn, not the newest return, orders the two: a
/// background ending publishes its notice before the return is written, so a
/// return landing after the notice must not make that ending look unreported.
/// Both writers read this one rule, so an ending the reconciliation pass
/// announced is not announced again by the queue drain.
pub fn completion_notice_pending(log: &[SessionLogEntry], child: &str, topic: &str) -> bool {
    let visible = &log[last_clear_index(log).unwrap_or(0)..];
    let mut newest_spawn = None;
    let mut newest_notice = None;
    for (index, entry) in visible.iter().enumerate() {
        match &entry.event {
            SessionEvent::SubagentSpawn {
                child_session_id, ..
            } if child_session_id == child => {
                newest_spawn = Some(index);
            }
            SessionEvent::NotificationInjected {
                child_session_id,
                topic: seen,
                ..
            } if child_session_id == child && seen == topic => {
                newest_notice = Some(index);
            }
            _ => {}
        }
    }
    match (newest_spawn, newest_notice) {
        // The window holds a notice but no spawn to order it against, so the
        // notice already reports whatever ending it named.
        (None, Some(_)) => false,
        (Some(spawned), Some(notice)) => notice < spawned,
        _ => true,
    }
}

/// Append the completion notice a live background child's ending would have
/// queued, so the model learns of the orphan at its next turn boundary.
///
/// The append is skipped when the log already holds a notice for this child's
/// newest ending: a pass that closed the boundary but could not finish is
/// resumed on the next entry, and the model reads one notice, not one per
/// pass.
async fn append_notice(
    store: &dyn SessionLog,
    parent_sid: SessionId,
    child: &str,
    notice: &str,
) -> Result<(), SpawnError> {
    let event = new_event(
        parent_sid,
        SessionEvent::NotificationInjected {
            child_session_id: child.to_string(),
            turn: 0,
            order: 0,
            topic: NotificationTopic::ChildCompletion.as_word().to_string(),
            summary: notice.to_string(),
            capture: None,
        },
    );
    let topic = NotificationTopic::ChildCompletion.as_word().to_string();
    let child_id = child.to_string();
    let unannounced = {
        let child_id = child_id.clone();
        let topic = topic.clone();
        move |log: &[SessionLogEntry]| completion_notice_pending(log, &child_id, &topic)
    };
    store
        .append_if(event, &unannounced)
        .await
        .map(|_| ())
        .map_err(|error| {
            warn!(?error, child = %child_id, "orphaned child notice append failed");
            SpawnError::BoundaryWriteFailed
        })
}

/// Append one return boundary. The caller holds the claim on this child, so
/// the write itself needs no condition.
async fn write_return(
    store: &dyn SessionLog,
    parent_sid: SessionId,
    record: &ReturnRecord<'_>,
) -> Result<(), SpawnError> {
    store
        .append(return_event(parent_sid, record))
        .await
        .map(|_| ())
        .map_err(|error| {
            warn!(
                ?error,
                child = %record.child_session_id,
                "subagent return append failed"
            );
            SpawnError::BoundaryWriteFailed
        })
}

/// Append a return for a boundary an earlier host left open, reporting
/// whether it was written.
///
/// This runs once per restart over the few boundaries a crash left open, so it
/// can afford to re-read the parent log under the append lock and settle the
/// boundary against it: a live ending that landed between the scan's read and
/// this write keeps the boundary, and the scan writes nothing it does not own.
async fn settle_open_return(
    store: &dyn SessionLog,
    parent_sid: SessionId,
    record: &ReturnRecord<'_>,
) -> Result<bool, SpawnError> {
    let child = record.child_session_id.to_string();
    let open = move |log: &[SessionLogEntry]| has_open_spawn(log, &child);
    let written = store
        .append_if(return_event(parent_sid, record), &open)
        .await
        .map_err(|error| {
            warn!(
                ?error,
                child = %record.child_session_id,
                "orphaned return append failed"
            );
            SpawnError::BoundaryWriteFailed
        })?;
    if written.is_none() {
        tracing::debug!(
            child = %record.child_session_id,
            "the boundary was settled before the orphan return landed"
        );
    }
    Ok(written.is_some())
}

/// The durable return event one record describes.
fn return_event(parent_sid: SessionId, record: &ReturnRecord<'_>) -> SessionLogEntry {
    let ledger = record.ledger.unwrap_or_default();
    new_event(
        parent_sid,
        SessionEvent::SubagentReturn {
            child_session_id: record.child_session_id.to_string(),
            status: record.status.to_string(),
            summary: record.summary.to_string(),
            result_ref: record.result_ref.to_string(),
            input_tokens: record.usage.input_tokens as u64,
            output_tokens: record.usage.output_tokens as u64,
            cache_read_input_tokens: record.usage.cache_read_input_tokens as u64,
            cache_write_input_tokens: record.usage.cache_write_input_tokens as u64,
            reasoning_tokens: record.usage.reasoning_tokens as u64,
            input_reported: record.usage.provenance.input_reported,
            cache_read_reported: record.usage.provenance.cache_read_reported,
            cache_write_reported: record.usage.provenance.cache_write_reported,
            tool_calls: ledger.tool_calls,
            tool_failures: ledger.tool_failures,
            duration_ms: ledger.duration_ms,
            ledger_measured: record.ledger.is_some(),
            orphaned: record.orphaned,
        },
    )
}

/// Whether this child still has a spawn that no return has settled, counted in
/// event order: a resumed delegation writes a second spawn after the first
/// return, so a return alone does not close a later spawn.
fn has_open_spawn(log: &[SessionLogEntry], child: &str) -> bool {
    let mut depth = 0u32;
    for entry in log {
        match &entry.event {
            SessionEvent::SubagentSpawn {
                child_session_id, ..
            } if child_session_id == child => depth += 1,
            SessionEvent::SubagentReturn {
                child_session_id, ..
            } if child_session_id == child => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth > 0
}
