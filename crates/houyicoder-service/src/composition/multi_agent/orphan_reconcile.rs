//! Reconciling delegation boundaries a host restart left open.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_api::session::{SessionLog, last_clear_index};
use houyicoder_context::{NotificationTopic, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_core::agent::multi_agent::spawn::TriggerSource;
use houyicoder_core::agent::multi_agent::{
    BoundaryOutcome, ChildLedger, OrphanedChild, RecordOwed, completion_notice_pending,
    logged_usage, orphaned_summary, record_orphaned_child,
};
use houyicoder_protocol::frontend::completion::CompletionStatus;

use crate::composition::notification_drain::notification_text;

use super::BoundaryClaims;
use super::children::ChildEntry;
use super::last_assistant_text;

/// The interruption a boundary left open reports. The child's log holds no
/// terminal record, so the record names what the delegation observed — the
/// host stopped before the ending was written — rather than asserting how the
/// child itself finished.
const INTERRUPTED: &str = "interrupted";

/// The generations that decide whether a scan for boundaries a previous host
/// left open is due.
///
/// A scan records the generation it read once every boundary it found is
/// written. A write failure bumps the request counter instead of clearing a
/// flag, so a scan already in flight cannot record its generation over the
/// reopen: its final read sees the new counter and leaves the next entry the
/// scan.
#[derive(Clone)]
pub(super) struct ScanGeneration {
    /// The generation requested. Starts one ahead of the recorded one so the
    /// first entry scans.
    requested: Arc<AtomicU64>,
    /// The generation the last completed scan read.
    scanned: Arc<AtomicU64>,
}

impl Default for ScanGeneration {
    fn default() -> Self {
        Self {
            requested: Arc::new(AtomicU64::new(1)),
            scanned: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl ScanGeneration {
    /// Whether a scan is due: no completed scan has read the requested
    /// generation.
    fn needs_scan(&self) -> bool {
        self.scanned.load(Ordering::Acquire) != self.requested.load(Ordering::Acquire)
    }

    /// The generation this scan reads, to be recorded when it completes.
    fn begin(&self) -> u64 {
        self.requested.load(Ordering::Acquire)
    }

    /// Record the scan as complete, unless a write failure reopened it while
    /// this pass ran.
    fn record(&self, generation: u64) {
        if self.requested.load(Ordering::Acquire) == generation {
            self.scanned.store(generation, Ordering::Release);
        }
    }

    /// Let a later entry reconcile again after a boundary write failed or a
    /// live writer was refused the claim.
    pub(super) fn reopen(&self) {
        self.requested.fetch_add(1, Ordering::AcqRel);
    }
}

/// One delegate the parent log records, tracked in event order.
#[derive(Default)]
struct DelegationRecord {
    /// Spawns seen minus returns seen for this child, floored at zero.
    depth: u32,
    /// The most recent spawn's subagent type, for the completion notice.
    subagent_type: String,
    /// The model call that started the most recent spawn, when the model
    /// started it.
    call_id: Option<String>,
    /// Whether the newest return is one a reconciliation pass wrote. A
    /// boundary settled that way may still owe the model its record, because
    /// the pass that closed it can stop before writing that record.
    orphaned: bool,
    /// The newest return's status word.
    status: String,
    /// The newest return's summary.
    summary: String,
    /// Whether the newest spawn left a child running after its call returned.
    /// A background launch answers its own call at once, so the call's result
    /// is the launch acknowledgement and the child's ending reaches the model
    /// only as the completion notice. A spawn no call started records no call
    /// id, so it never reads as a background launch and a lost notice for such
    /// a child is not restored; no production path starts a spawn that way.
    launched_in_background: bool,
    /// Whether the model's epoch holds this child's completion notice.
    notice_delivered: bool,
}

/// What the parent log says about every delegation it holds.
#[derive(Default)]
struct ParentLog {
    /// One record per child, folded in event order.
    delegations: HashMap<String, DelegationRecord>,
    /// Delegation calls in the model's current epoch.
    visible_calls: HashSet<String>,
    /// Calls the model's current epoch answered.
    answered_calls: HashSet<String>,
}

/// Fold the parent log into what each delegation still owes.
///
/// Spawns and returns are matched in event order over the whole log: a
/// resumed delegation writes a second spawn after the first return, so an id
/// with any return is not settled. The call sets cover the model's current
/// epoch only, because an answer placed outside it never reaches the model.
/// Whether the model holds a child's ending follows the same rule the notice
/// writers use, so a notice written before a resumed spawn does not count for
/// that run.
fn fold(log: &[SessionLogEntry]) -> ParentLog {
    let mut folded = ParentLog::default();
    let visible = &log[last_clear_index(log).unwrap_or(0)..];
    for entry in visible {
        match &entry.event {
            SessionEvent::ToolCall { call_id, .. } => {
                folded.visible_calls.insert(call_id.clone());
            }
            SessionEvent::ToolResult { call_id, .. } => {
                folded.answered_calls.insert(call_id.clone());
            }
            _ => {}
        }
    }
    // Fold the delegation boundaries in event order. The launch mode travels
    // on the spawn event itself, so the return does not need to infer it from
    // where the tool result landed.
    for entry in log.iter() {
        match &entry.event {
            SessionEvent::SubagentSpawn {
                child_session_id,
                subagent_type,
                trigger_source,
                run_in_background,
                ..
            } => {
                let delegation = folded
                    .delegations
                    .entry(child_session_id.clone())
                    .or_default();
                delegation.depth = delegation.depth.saturating_add(1);
                delegation.subagent_type = subagent_type.clone();
                delegation.call_id = match TriggerSource::from_durable(trigger_source) {
                    Some(TriggerSource::ModelTool { tool_call_id }) => Some(tool_call_id),
                    _ => None,
                };
                delegation.orphaned = false;
                delegation.status.clear();
                delegation.summary.clear();
                // The launch mode is read from the boundary, not inferred from
                // the order of the tool result and the return. A foreground
                // child interrupted by the user also writes its result before
                // its return, which a position heuristic would mistake for a
                // background launch and re-notify on restart.
                delegation.launched_in_background = *run_in_background;
            }
            SessionEvent::SubagentReturn {
                child_session_id,
                status,
                summary,
                orphaned,
                ..
            } => {
                if let Some(delegation) = folded.delegations.get_mut(child_session_id) {
                    delegation.depth = delegation.depth.saturating_sub(1);
                    delegation.orphaned = *orphaned;
                    delegation.status = status.clone();
                    delegation.summary = summary.clone();
                }
            }
            _ => {}
        }
    }
    for (child, record) in folded.delegations.iter_mut() {
        record.notice_delivered =
            !completion_notice_pending(log, child, NotificationTopic::ChildCompletion.as_word());
    }
    folded
}

/// The record one delegation still owes the model, or None when it owes none.
///
/// An open boundary always owes its return, and its answer follows when the
/// call is still unanswered in view; a launch that already answered its call
/// gets the notice instead. A boundary a live ending settled owes the model's
/// result when the host stopped between that return and the result, or the
/// completion notice when the child ran in the background and the host
/// stopped before the parent's next boundary drained that notice. A boundary
/// a pass closed owes whatever that pass did not finish.
fn owed_record(
    record: &DelegationRecord,
    call_visible: bool,
    call_answered: bool,
) -> Option<RecordOwed<'_>> {
    if record.depth > 0 {
        return Some(match record.call_id.as_deref() {
            // A launch that answered its own call keeps that result: the
            // ending reaches the model as the notice every background child
            // sends.
            Some(call_id) if call_visible && !call_answered => RecordOwed::Answer { call_id },
            _ => RecordOwed::Notice,
        });
    }
    if record.orphaned {
        return match record.call_id.as_deref() {
            Some(call_id) if call_visible && !call_answered => Some(RecordOwed::Answer { call_id }),
            // A pass can write the return and stop before the notice. When
            // the child ran in the background its call was answered by the
            // launch acknowledgement, so that answer does not report the
            // ending and the notice is still owed.
            _ if record.launched_in_background && call_answered && !record.notice_delivered => {
                Some(RecordOwed::Notice)
            }
            // The call left the model's window, so the notice is the record.
            _ if !call_visible && !record.notice_delivered => Some(RecordOwed::Notice),
            _ => None,
        };
    }
    match record.call_id.as_deref() {
        Some(call_id) if call_visible && !call_answered => Some(RecordOwed::Answer { call_id }),
        // A background launch's answer is its launch acknowledgement, not the
        // child's result; the ending arrives as the completion notice, and a
        // host that stopped before the parent's next boundary drained it owes
        // the model that notice.
        _ if record.launched_in_background && call_answered && !record.notice_delivered => {
            Some(RecordOwed::Notice)
        }
        _ => None,
    }
}

/// Close every delegation boundary an earlier host left unfinished, and write
/// the record the model reads about each.
///
/// The scan reads the durable parent log rather than the resident tail: the
/// window a restarted process holds starts empty, so a tail read would find no
/// spawns at all on the very run this exists for, and a child's text is read
/// the same durable way for the same reason.
///
/// The return carries the child's last assistant text and the usage its log
/// records, and the model reads either the answer to its unanswered delegation
/// call or the completion notice, so the work can continue under the same
/// child id rather than being paid for again.
///
/// Runs at most one completed scan per generation. A runtime serves one parent
/// session, so the first entry after a restart is the only one that can find
/// an orphan, and rescanning the durable parent log on every turn would cost a
/// full read for no new answer. A scan records the generation it read only
/// once every boundary was resolved: a run dropped mid-scan, a parent-log read
/// that failed, a boundary another writer holds, a child that started during
/// the scan, or a write that failed leaves the next entry to retry, because a
/// rescan that finds nothing open writes nothing. A live return write that
/// fails, and a claim it is refused, bump the generation so the failure has
/// one owner and a refused writer is not the last word on the boundary.
///
/// The return is claimed before it is written, so a live ending and this scan
/// cannot both write one, and the scan's own append is conditional on the
/// spawn still being open, so a live ending that landed after the scan's read
/// keeps its boundary. That condition costs one parent-log replay per open
/// boundary, paid once on the entry after a restart; the live path writes its
/// return under its claim without reading the log back.
pub(super) async fn run_once(
    scan: &ScanGeneration,
    store: &Arc<dyn SessionLog>,
    children: &Arc<Mutex<HashMap<String, ChildEntry>>>,
    claims: &BoundaryClaims,
    parent_sid: SessionId,
) {
    if !scan.needs_scan() {
        return;
    }
    let scanning = scan.begin();
    let log = match store.replay(parent_sid).await {
        Ok(log) => log,
        Err(error) => {
            tracing::warn!(
                ?error,
                "orphan reconciliation could not read the parent log"
            );
            return;
        }
    };
    let ParentLog {
        delegations,
        visible_calls,
        answered_calls,
        ..
    } = fold(&log);
    let live = live_children(children);
    let mut closed = 0u32;
    let mut every_boundary_resolved = true;
    for (child, record) in delegations {
        // A running child owns its own ending, and a live ending writes the
        // record the model reads.
        if live.contains(&child) {
            continue;
        }
        let call_visible = record
            .call_id
            .as_ref()
            .is_some_and(|call| visible_calls.contains(call));
        let call_answered = record
            .call_id
            .as_ref()
            .is_some_and(|call| answered_calls.contains(call));
        let Some(owed) = owed_record(&record, call_visible, call_answered) else {
            continue;
        };
        let outcome =
            close_boundary(store, parent_sid, claims, children, &child, &record, owed).await;
        if matches!(&outcome, CloseOutcome::Closed) {
            closed = closed.saturating_add(1);
        }
        // A child whose boundary this pass backed out of, a claim another
        // writer holds, and a failed write all leave the boundary unsettled,
        // so a later entry has to look again. A child that started during the
        // scan works the same way: its live ending may have reached its own
        // claim between this pass taking one and checking liveness, and this
        // pass then backs out with the claim released.
        if matches!(
            &outcome,
            CloseOutcome::Claimed | CloseOutcome::Failed | CloseOutcome::Running
        ) {
            every_boundary_resolved = false;
        }
    }
    tracing::debug!(closed, "orphan reconciliation pass completed");
    // A scan that ran while a boundary write failed has to leave the next
    // entry the scan, so recording checks the requested epoch again rather
    // than trusting the snapshot taken above.
    if every_boundary_resolved {
        scan.record(scanning);
    }
}

/// The children whose runner is still alive.
///
/// A poisoned lock must not read as "no child is running": that would
/// reconcile live children and write a second return for work still owed.
/// Recover the guard the writer left, the way the sibling join does.
fn live_children(children: &Arc<Mutex<HashMap<String, ChildEntry>>>) -> HashSet<String> {
    let guard = match children.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .iter()
        .filter(|(_, entry)| entry.runner.upgrade().is_some())
        .map(|(id, _)| id.clone())
        .collect()
}

/// Whether this child's runner is alive right now. Read after a claim, so a
/// child that started during the scan keeps its own ending.
fn is_live(children: &Arc<Mutex<HashMap<String, ChildEntry>>>, child: &str) -> bool {
    let guard = match children.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .get(child)
        .is_some_and(|entry| entry.runner.upgrade().is_some())
}

/// How one child's boundary came out of a pass.
enum CloseOutcome {
    /// This pass wrote the boundary or the record it owed.
    Closed,
    /// A live ending settled the boundary first and owns the model's record.
    Settled,
    /// The child is running and owns its own ending. This pass releases the
    /// claim it took, and the entry reports the boundary unresolved, because
    /// the live ending may have been refused that claim in between.
    Running,
    /// Another writer holds the claim, so a later entry must retry.
    Claimed,
    /// The child id is malformed, so no pass can close it.
    Unusable,
    /// The write failed and a later entry must retry it.
    Failed,
}

/// Close one boundary the parent log records, including the record the model
/// reads about it.
///
/// A boundary that is already settled needs only the model's record, and its
/// append is conditional, so two passes may retry it at once. The return for
/// an open boundary is written once, by the writer that claims it.
async fn close_boundary(
    store: &Arc<dyn SessionLog>,
    parent_sid: SessionId,
    claims: &BoundaryClaims,
    children: &Arc<Mutex<HashMap<String, ChildEntry>>>,
    child: &str,
    record: &DelegationRecord,
    owed: RecordOwed<'_>,
) -> CloseOutcome {
    let boundary_open = record.depth > 0;
    // The guard releases the claim on every exit, including a cancelled pass.
    let _claim = if boundary_open {
        let Some(held) = claims.claim(child) else {
            return CloseOutcome::Claimed;
        };
        // The child may have started between the scan's read of the log and
        // this claim. A running child owns its own ending, so the scan backs
        // out with the claim released.
        if is_live(children, child) {
            return CloseOutcome::Running;
        }
        Some(held)
    } else {
        None
    };
    // The id is validated for both paths: a settled boundary writes only the
    // notice, and its text shortens this id.
    let Some(child_sid) = SessionId::from_display_string(child) else {
        tracing::warn!(%child, "orphan reconciliation skipped an unparseable child id");
        return CloseOutcome::Unusable;
    };
    let (partial, ledger, usage) = if boundary_open {
        match store.replay(child_sid).await {
            Ok(log) => (
                Some(last_assistant_text(&log).unwrap_or_default()),
                Some(ChildLedger::from_log(&log)),
                Some(logged_usage(&log)),
            ),
            Err(error) => {
                // The boundary still has to close, so it is written without
                // the child's text and figures; the warning names the read
                // that failed, and the return reports the figures as unknown.
                tracing::warn!(?error, %child, "orphan reconciliation could not read the child log");
                (None, None, None)
            }
        }
    } else {
        // A settled boundary needs no return, so its text and figures would be
        // read only to be dropped; the notice reads the summary the return
        // already carries.
        (None, None, None)
    };
    let status_word = if boundary_open {
        INTERRUPTED
    } else {
        record.status.as_str()
    };
    let summary = if boundary_open {
        orphaned_summary(child, partial.as_deref())
    } else {
        record.summary.clone()
    };
    let notice_summary = if boundary_open {
        match &partial {
            None => "the child's log could not be read, so its partial answer is lost".to_string(),
            Some(partial) if partial.is_empty() => {
                "the host restarted before it produced an answer".to_string()
            }
            Some(partial) => partial.clone(),
        }
    } else {
        record.summary.clone()
    };
    let notice = notification_text(
        &record.subagent_type,
        child,
        &CompletionStatus::from_word(status_word),
        &notice_summary,
    );
    let found = OrphanedChild {
        child_session_id: child,
        boundary_open,
        status: status_word,
        summary: &summary,
        record: owed,
        usage,
        ledger,
        notice: &notice,
    };
    match record_orphaned_child(store.as_ref(), parent_sid, &found).await {
        Ok(BoundaryOutcome::Closed) => CloseOutcome::Closed,
        Ok(BoundaryOutcome::Settled) => CloseOutcome::Settled,
        Err(error) => {
            tracing::warn!(?error, %child, "orphan reconciliation boundary write failed");
            CloseOutcome::Failed
        }
    }
}
