//! State and transitions for the /tasks pane.
//!
//! The selection names a task, not a row: the list reorders as tasks arrive
//! and end, and an action must reach the task the user highlighted. The host
//! owns every task, so each read replaces the record held here; a running
//! task's detail reads again a second later, and an ended one stops.

use std::cell::Cell;
use std::time::{Duration, Instant};

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::tasks::{BackgroundTaskDetail, BackgroundTaskId};

/// How long the pane waits between two reads of the same source.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The open task detail: the number it belongs to, the newest record the
/// host sent, the scroll position, and the read that keeps it current.
pub(crate) struct TaskDetail {
    id: BackgroundTaskId,
    record: Option<BackgroundTaskDetail>,
    offset: Cell<u16>,
    max_offset: Cell<u16>,
    pending: Option<RequestId>,
    next_read: Option<Instant>,
}

impl TaskDetail {
    /// The number this detail belongs to.
    pub(crate) fn id(&self) -> BackgroundTaskId {
        self.id
    }

    /// The newest record the host sent, absent until the first reply lands.
    pub(crate) fn record(&self) -> Option<&BackgroundTaskDetail> {
        self.record.as_ref()
    }

    /// The scroll position the view clamps.
    pub(crate) fn offset(&self) -> u16 {
        self.offset.get()
    }

    fn ended(&self) -> bool {
        self.record
            .as_ref()
            .is_some_and(|record| record.status.is_terminal())
    }
}

/// The list selection plus the open detail and the pending reads for the
/// /tasks pane.
#[derive(Default)]
pub(crate) struct TasksPaneState {
    /// The task the list acts on. It stays unset until the user moves, which
    /// resolves to the first row: the newest running task, or the newest
    /// ended one when nothing runs.
    selected: Cell<Option<BackgroundTaskId>>,
    detail: Option<TaskDetail>,
    list_read: Cell<Option<Instant>>,
}

impl TasksPaneState {
    /// The row the selection stands on in this ordering. The task it names
    /// keeps its place as rows move around it; once that task leaves the
    /// list, the first row takes the selection, so the highlight and the
    /// action never disagree.
    pub(crate) fn selected_row(&self, rows: &[BackgroundTaskId]) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        Some(
            self.selected
                .get()
                .and_then(|id| rows.iter().position(|row| *row == id))
                .unwrap_or(0),
        )
    }

    /// Move the selection by a signed delta, clamped to the list bounds. A
    /// no-op on an empty list, where there is no row to point at.
    pub(crate) fn move_cursor(&self, delta: i32, rows: &[BackgroundTaskId]) {
        let Some(current) = self.selected_row(rows) else {
            self.selected.set(None);
            return;
        };
        let next = (current as i32 + delta).clamp(0, (rows.len() - 1) as i32) as usize;
        self.selected.set(Some(rows[next]));
    }

    /// Put the selection back on the first row, so a pane opened again shows
    /// its newest running task rather than wherever the last visit stopped.
    pub(crate) fn reset_selection(&self) {
        self.selected.set(None);
    }

    /// The open detail, when the pane is one level down.
    pub(crate) fn detail(&self) -> Option<&TaskDetail> {
        self.detail.as_ref()
    }

    /// The number the pane acts on: the open detail's, or the list's
    /// selected row. Absent on an empty list.
    pub(crate) fn target(&self, rows: &[BackgroundTaskId]) -> Option<BackgroundTaskId> {
        match self.detail.as_ref() {
            Some(detail) => Some(detail.id),
            None => self.selected_row(rows).map(|index| rows[index]),
        }
    }

    /// Open one task's detail and mark its first read as in flight. The reply
    /// is matched by request id, so a reply for an earlier open does not fill
    /// this one.
    pub(crate) fn open_detail(&mut self, id: BackgroundTaskId, request: RequestId, now: Instant) {
        self.detail = Some(TaskDetail {
            id,
            record: None,
            offset: Cell::new(0),
            max_offset: Cell::new(0),
            pending: Some(request),
            next_read: Some(now + POLL_INTERVAL),
        });
    }

    /// Return to the list. The read schedule goes with the detail, so a closed
    /// detail costs no traffic.
    pub(crate) fn close_detail(&mut self) {
        self.detail = None;
    }

    /// Fill the open detail with the record a reply carried. Returns false
    /// when no detail is open or the reply belongs to an earlier read, so a
    /// stale reply cannot replace a newer record.
    pub(crate) fn apply_detail(
        &mut self,
        request: RequestId,
        record: BackgroundTaskDetail,
    ) -> bool {
        let Some(detail) = self.detail.as_mut() else {
            return false;
        };
        if detail.pending != Some(request) {
            return false;
        }
        detail.pending = None;
        // An ended task has no further state to read; the last screen keeps
        // its ending and exit status.
        if record.status.is_terminal() {
            detail.next_read = None;
        }
        detail.record = Some(record);
        true
    }

    /// Settle a read the host answered with an error. The last record stays
    /// on screen and the schedule stops rather than retrying a read the host
    /// has already refused.
    pub(crate) fn fail_read(&mut self, request: RequestId) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        if detail.pending != Some(request) {
            return;
        }
        detail.pending = None;
        detail.next_read = None;
    }

    /// Settle a read the connection can no longer answer. The last record
    /// stays on screen and both schedules drop: there is no connection left
    /// to read from, and a schedule that stayed set would retry every second
    /// against a dead connection.
    pub(crate) fn clear_pending(&mut self) {
        if let Some(detail) = self.detail.as_mut() {
            detail.pending = None;
            detail.next_read = None;
        }
        self.list_read.set(None);
    }

    /// Start the list read schedule at the moment the pane opens; the first
    /// query is sent by the open path itself.
    pub(crate) fn begin_list_read(&mut self, now: Instant) {
        self.list_read.set(Some(now + POLL_INTERVAL));
    }

    /// Whether the list is due for another read.
    pub(crate) fn list_due(&self, now: Instant) -> bool {
        self.list_read.get().is_some_and(|due| now >= due)
    }

    /// Record that a list read went out now.
    pub(crate) fn mark_list_read(&mut self, now: Instant) {
        self.list_read.set(Some(now + POLL_INTERVAL));
    }

    /// Stop reading the list, for a pane that closed while open.
    pub(crate) fn stop_list_read(&mut self) {
        self.list_read.set(None);
    }

    /// The task whose detail is due for another read: one that has not ended,
    /// whose previous read answered, and whose interval passed.
    pub(crate) fn detail_due(&self, now: Instant) -> Option<BackgroundTaskId> {
        let detail = self.detail.as_ref()?;
        if detail.pending.is_some() || detail.ended() {
            return None;
        }
        detail
            .next_read
            .is_some_and(|due| now >= due)
            .then_some(detail.id)
    }

    /// Record that a detail read went out now.
    pub(crate) fn mark_detail_read(&mut self, request: RequestId, now: Instant) {
        if let Some(detail) = self.detail.as_mut() {
            detail.pending = Some(request);
            detail.next_read = Some(now + POLL_INTERVAL);
        }
    }

    /// Record the largest scroll offset the last render produced, clamping
    /// the current offset so a shorter record cannot leave the view past its
    /// end.
    pub(crate) fn set_detail_max_offset(&self, max_offset: u16) {
        if let Some(detail) = self.detail.as_ref() {
            detail.max_offset.set(max_offset);
            detail.offset.set(detail.offset.get().min(max_offset));
        }
    }

    /// Scroll the open detail by a signed delta, clamped to the offset the
    /// last render measured.
    pub(crate) fn scroll_detail(&self, delta: i16) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let next = if delta.is_negative() {
            detail.offset.get().saturating_sub(delta.unsigned_abs())
        } else {
            detail.offset.get().saturating_add(delta.unsigned_abs())
        };
        detail.offset.set(next.min(detail.max_offset.get()));
    }

    #[cfg(test)]
    pub(crate) fn detail_record(&self) -> Option<&BackgroundTaskDetail> {
        self.detail
            .as_ref()
            .and_then(|detail| detail.record.as_ref())
    }

    #[cfg(test)]
    pub(crate) fn offset(&self) -> u16 {
        self.detail.as_ref().map_or(0, TaskDetail::offset)
    }
}

#[cfg(test)]
mod tests;
