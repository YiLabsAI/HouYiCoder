//! The /mcp pane state: the list cursor, the open detail with its scroll,
//! the status rows the host returned, and the reply each action waits on.

use std::cell::Cell;
use std::time::Duration;
use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::mcp::McpConnectionState;
use houyicoder_protocol::mcp::McpServerStatusView;

/// How often the pane asks again while a row the host reported is still
/// settling (starting, reconnecting). A reply reflects the manager's state
/// at the instant it was read, and a handshake or a relaunch finishes later
/// with nothing pushing it back, so the pane polls until the rows settle.
pub(crate) const MCP_REFRESH_INTERVAL: Duration = Duration::from_millis(250);

/// How many follow-up asks one action may spend before the pane stops
/// polling. The host's own handshake window is the horizon a row can stay
/// pending inside; past that a wedged server stays as the host reported it.
pub(crate) const MCP_REFRESH_BUDGET: u16 = 60;

/// The receipt one toggle writes to the transcript once the host confirms
/// it: which server moved and to which state.
#[derive(Debug, Clone)]
pub(crate) struct McpToggleReceipt {
    pub name: String,
    pub enabled: bool,
}

/// What the pane is waiting on one reply for: the request it must match, and
/// the toggle whose confirmation the reply also reports.
#[derive(Debug, Clone)]
pub(crate) struct McpPending {
    pub request: RequestId,
    pub toggle: Option<McpToggleReceipt>,
}

/// The pane's interaction state. Level 0 is the list; level 1 is one
/// server's detail. The cursor and the detail level move together on Enter
/// and Esc, matching the sibling panes' grammar.
#[derive(Debug, Default)]
pub(crate) struct McpPaneState {
    /// 0 = the list; 1 = the open detail.
    pub level: u8,
    /// The selected row in the list, or the row the detail shows.
    pub cursor: usize,
    /// The status rows the host returned, or an empty list before the first
    /// reply lands.
    pub rows: Vec<McpServerStatusView>,
    /// The reply being waited on, so a late or stale one changes nothing.
    pub pending: Option<McpPending>,
    /// When the next settling poll is due, while one is armed.
    refresh_at: Option<Instant>,
    /// How many settling polls one action may still spend.
    refresh_left: u16,
    /// The detail body's scroll position, clamped by the last render's
    /// published maximum.
    detail_offset: Cell<u16>,
    detail_max: Cell<u16>,
}

impl McpPaneState {
    /// The row the cursor stands on, or None on an empty list.
    pub(crate) fn selected(&self) -> Option<&McpServerStatusView> {
        self.rows.get(self.cursor)
    }

    /// Replace the status rows from a host reply. A reply whose request does
    /// not match the pending one changes nothing; a matching one settles the
    /// pending action and hands back its receipt, so the confirmation and
    /// the refreshed rows land on the same reply. The cursor stays in range,
    /// clamped to the new list length.
    pub(crate) fn apply_status(
        &mut self,
        request: RequestId,
        servers: Vec<McpServerStatusView>,
    ) -> Option<McpToggleReceipt> {
        if self.pending.as_ref().is_none_or(|p| p.request != request) {
            return None;
        }
        let receipt = self.pending.take().and_then(|p| p.toggle);
        self.rows = servers;
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        // A row the host still reports as settling finishes later with no
        // reply of its own, so arm a follow-up ask for the states a
        // handshake or relaunch is heading through.
        let settling = self.rows.iter().any(|row| {
            matches!(
                row.state,
                McpConnectionState::Pending | McpConnectionState::Reconnecting { .. }
            )
        });
        self.refresh_at = settling
            .then(|| Instant::now() + MCP_REFRESH_INTERVAL)
            .filter(|_| self.refresh_left > 0);
        receipt
    }

    /// Mark a read-only status query as pending. Each user action gets a
    /// fresh settling budget, so a new request is not starved by a spent
    /// one.
    pub(crate) fn mark_pending(&mut self, request: RequestId) {
        self.pending = Some(McpPending {
            request,
            toggle: None,
        });
        self.refresh_left = MCP_REFRESH_BUDGET;
    }

    /// Mark a toggle as pending, remembering what its confirmation should
    /// tell the transcript.
    pub(crate) fn mark_pending_toggle(&mut self, request: RequestId, name: String, enabled: bool) {
        self.pending = Some(McpPending {
            request,
            toggle: Some(McpToggleReceipt { name, enabled }),
        });
        self.refresh_left = MCP_REFRESH_BUDGET;
    }

    /// Mark a settling follow-up ask as pending without refilling its
    /// budget: a poll the budget charged for must not pay itself back, or
    /// the cap would never bind.
    pub(crate) fn mark_settling_pending(&mut self, request: RequestId) {
        self.pending = Some(McpPending {
            request,
            toggle: None,
        });
    }

    /// Whether a settling follow-up ask is still armed: some row is settling
    /// and the budget has not run out. The event loop reads this to keep its
    /// wake cadence; a settled pane arms nothing.
    pub(crate) fn refresh_armed(&self) -> bool {
        self.refresh_at.is_some() && self.refresh_left > 0
    }

    /// Whether a settling follow-up ask is due at this instant, spending one
    /// of the action's budget when it is. False once the budget is spent or
    /// no row is still settling, so an idle pane polls nothing.
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

    /// Settle a pending action the host refused, keeping the rows the pane
    /// already holds. True when the request was the pane's.
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
    /// A no-op on an empty list.
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

    /// Whether the selected row's server may be asked to reconnect: a
    /// disabled server has nothing to reconnect, whether the user or an
    /// untrusted workspace turned it off.
    pub(crate) fn allows_reconnect(&self) -> bool {
        self.selected()
            .is_some_and(|row| !matches!(row.state, McpConnectionState::Disabled { .. }))
    }

    /// Whether the selected row's server may be toggled: a server an
    /// untrusted workspace locked stays off until the workspace is trusted,
    /// so the pane refuses to offer the key at all.
    pub(crate) fn allows_toggle(&self) -> bool {
        self.selected()
            .is_some_and(|row| !row.detail.untrusted_lock)
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
