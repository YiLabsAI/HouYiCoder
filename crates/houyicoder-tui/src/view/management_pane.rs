//! The interaction core the management panes share: the list cursor and
//! detail level, the pending-reply accounting, the settle-poll cadence, and
//! the detail scroll clamp. A pane supplies its row type, the predicate that
//! says a row is still settling, and the receipt its actions hand back, so
//! the sibling panes keep one grammar instead of drifting copies.

use std::cell::Cell;
use std::time::Duration;
use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;

/// How often a pane asks again while a row the host reported is still
/// settling. A reply reflects the host's state at the instant it was read,
/// and a handshake or a relaunch finishes later with nothing pushing it
/// back, so the pane polls until the rows settle.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

/// How many follow-up asks one action may spend before the pane stops
/// polling, so a wedged row is reported as it stands rather than polled
/// forever.
pub(crate) const REFRESH_BUDGET: u16 = 60;

/// What a pane waits on one reply for: the request it must match, the
/// receipt its action hands back, and whether the reply cannot be final
/// even when every row reads settled.
#[derive(Debug, Clone)]
pub(crate) struct Pending<Receipt> {
    pub request: RequestId,
    pub receipt: Option<Receipt>,
    pub expect_settle: bool,
}

/// One pane's interaction state. Level 0 is the list; level 1 is the open
/// detail. The cursor and the detail level move together on Enter and Esc.
#[derive(Debug)]
pub(crate) struct PaneMachine<Row, Receipt> {
    /// 0 = the list; 1 = the open detail.
    pub level: u8,
    /// The selected row in the list, or the row the detail shows.
    pub cursor: usize,
    /// The rows the host returned, or an empty list before the first reply.
    pub rows: Vec<Row>,
    /// The reply being waited on, so a late or stale one changes nothing.
    pub pending: Option<Pending<Receipt>>,
    refresh_at: Option<Instant>,
    refresh_left: u16,
    detail_offset: Cell<u16>,
    detail_max: Cell<u16>,
}

impl<Row, Receipt> Default for PaneMachine<Row, Receipt> {
    fn default() -> Self {
        Self {
            level: 0,
            cursor: 0,
            rows: Vec::new(),
            pending: None,
            refresh_at: None,
            refresh_left: 0,
            detail_offset: Cell::new(0),
            detail_max: Cell::new(0),
        }
    }
}

impl<Row, Receipt: Clone> PaneMachine<Row, Receipt> {
    /// The row the cursor stands on, or None on an empty list.
    pub(crate) fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    /// Replace the rows from a host reply. A reply whose request does not
    /// match the pending one changes nothing; a matching one settles the
    /// pending action and hands back its receipt, so the confirmation and
    /// the refreshed rows land on the same reply. settling names the states
    /// that finish later without a reply of their own.
    pub(crate) fn apply_status(
        &mut self,
        request: RequestId,
        rows: Vec<Row>,
        settling: impl Fn(&Row) -> bool,
    ) -> Option<Receipt> {
        if self.pending.as_ref().is_none_or(|p| p.request != request) {
            return None;
        }
        let settled = self.pending.take();
        let receipt = settled.as_ref().and_then(|p| p.receipt.clone());
        let expect_settle = settled.is_some_and(|p| p.expect_settle);
        self.rows = rows;
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        let unsettled = expect_settle || self.rows.iter().any(settling);
        self.refresh_at = unsettled
            .then(|| Instant::now() + REFRESH_INTERVAL)
            .filter(|_| self.refresh_left > 0);
        receipt
    }

    /// Mark a read-only status query as pending, with a fresh settling
    /// budget so a new request is not starved by a spent one.
    pub(crate) fn mark_pending(&mut self, request: RequestId) {
        self.begin(request, None, false);
    }

    /// Mark an action as pending. expect_settle makes the pane ask again
    /// even when the reply reads settled, because some replies are sent
    /// before the work they start has finished.
    pub(crate) fn mark_pending_action(
        &mut self,
        request: RequestId,
        receipt: Option<Receipt>,
        expect_settle: bool,
    ) {
        self.begin(request, receipt, expect_settle);
    }

    /// Mark a settling follow-up ask as pending without refilling its
    /// budget: a poll the budget charged for must not pay itself back, or
    /// the cap would never bind.
    pub(crate) fn mark_settling_pending(&mut self, request: RequestId) {
        self.pending = Some(Pending {
            request,
            receipt: None,
            expect_settle: false,
        });
    }

    fn begin(&mut self, request: RequestId, receipt: Option<Receipt>, expect_settle: bool) {
        self.pending = Some(Pending {
            request,
            receipt,
            expect_settle,
        });
        self.refresh_left = REFRESH_BUDGET;
    }

    /// Whether a settling follow-up ask is still armed. The event loop reads
    /// this to keep its wake cadence; a settled pane arms nothing.
    pub(crate) fn refresh_armed(&self) -> bool {
        self.refresh_at.is_some() && self.refresh_left > 0
    }

    /// Whether a settling follow-up ask is due at this instant, spending one
    /// of the action's budget when it is.
    pub(crate) fn take_due_refresh(&mut self, now: Instant) -> bool {
        if self.refresh_left == 0 {
            return false;
        }
        if self.refresh_at.is_none_or(|at| now < at) {
            return false;
        }
        self.refresh_left -= 1;
        self.refresh_at = None;
        true
    }

    /// Settle a pending action the host refused, keeping the rows. True when
    /// the request was the pane's.
    pub(crate) fn fail_pending(&mut self, request: RequestId) -> bool {
        if self.pending.as_ref().is_some_and(|p| p.request == request) {
            self.pending = None;
            self.refresh_at = None;
            true
        } else {
            false
        }
    }

    /// Move the list cursor by a signed delta, clamped to the list bounds.
    pub(crate) fn move_cursor(&mut self, delta: i32) {
        if self.rows.is_empty() {
            self.cursor = 0;
            return;
        }
        let current = self.cursor.min(self.rows.len() - 1) as i32;
        self.cursor = (current + delta).clamp(0, self.rows.len() as i32 - 1) as usize;
    }

    /// Open the detail for the selected row. An empty list has nothing to
    /// open, so the level stays on the list.
    pub(crate) fn open_detail(&mut self) {
        if self.selected().is_some() && self.level == 0 {
            self.level = 1;
            self.detail_offset.set(0);
        }
    }

    /// Return from the detail to the list.
    pub(crate) fn close_detail(&mut self) {
        self.level = 0;
    }

    /// Record the largest scroll offset the last render produced, clamping
    /// the current offset so a shorter body cannot leave the view past its
    /// end.
    pub(crate) fn set_detail_max_offset(&self, max_offset: u16) {
        self.detail_max.set(max_offset);
        self.detail_offset
            .set(self.detail_offset.get().min(max_offset));
    }

    /// Scroll the detail body by a signed line count, clamped to the
    /// published maximum.
    pub(crate) fn scroll_detail(&self, delta: i32) {
        let current = i32::from(self.detail_offset.get());
        let next = (current + delta).clamp(0, i32::from(self.detail_max.get()));
        self.detail_offset.set(u16::try_from(next).unwrap_or(0));
    }

    /// The detail body's scroll position.
    pub(crate) fn detail_offset(&self) -> u16 {
        self.detail_offset.get()
    }

    /// Clear the state for a session switch.
    pub(crate) fn reset(&mut self) {
        self.level = 0;
        self.cursor = 0;
        self.rows.clear();
        self.pending = None;
        self.refresh_at = None;
        self.refresh_left = 0;
        self.detail_offset.set(0);
        self.detail_max.set(0);
    }
}

#[cfg(test)]
mod tests;
