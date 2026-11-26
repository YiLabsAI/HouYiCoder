//! The /mcp pane state: the shared pane core over the external tool server
//! rows, plus the receipt a toggle hands the transcript and the row gating
//! the key handler consults.

use std::ops::Deref;
use std::ops::DerefMut;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::mcp::McpConnectionState;
use houyicoder_protocol::mcp::McpServerStatusView;

use crate::view::management_pane::PaneMachine;

#[cfg(test)]
pub(crate) use crate::view::management_pane::REFRESH_BUDGET as MCP_REFRESH_BUDGET;
#[cfg(test)]
pub(crate) use crate::view::management_pane::REFRESH_INTERVAL as MCP_REFRESH_INTERVAL;

/// The receipt one toggle writes to the transcript once the host confirms
/// it: which server moved, to which state, and whether the answer outlives
/// the session. Only a user-scope entry persists; a project or env entry
/// comes back the way its file spells it on the next start, so the receipt
/// must say the choice lasts this session.
#[derive(Debug, Clone)]
pub(crate) struct McpToggleReceipt {
    pub name: String,
    pub enabled: bool,
    pub session_only: bool,
}

impl McpToggleReceipt {
    /// The one-line transcript form: the change, plus the lifetime when the
    /// change does not outlive the session.
    pub(crate) fn line(&self) -> String {
        let state = if self.enabled { "enabled" } else { "disabled" };
        if self.session_only {
            format!("mcp: {} {state} (this session only)", self.name)
        } else {
            format!("mcp: {} {state}", self.name)
        }
    }
}

/// The pane's interaction state over the shared machine.
#[derive(Debug, Default)]
pub(crate) struct McpPaneState {
    machine: PaneMachine<McpServerStatusView, McpToggleReceipt>,
}

impl Deref for McpPaneState {
    type Target = PaneMachine<McpServerStatusView, McpToggleReceipt>;

    fn deref(&self) -> &Self::Target {
        &self.machine
    }
}

impl DerefMut for McpPaneState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.machine
    }
}

impl McpPaneState {
    /// Replace the rows from a host reply and settle the pending action.
    pub(crate) fn apply_status(
        &mut self,
        request: RequestId,
        servers: Vec<McpServerStatusView>,
    ) -> Option<McpToggleReceipt> {
        self.machine.apply_status(request, servers, |row| {
            matches!(
                row.state,
                McpConnectionState::Pending | McpConnectionState::Reconnecting { .. }
            )
        })
    }

    /// Mark a reconnect as pending. Its reply is answered before the fresh
    /// handshake runs, so the pane asks again once even if the reply reads
    /// settled, which is what makes the new connection observable.
    pub(crate) fn mark_pending_reconnect(&mut self, request: RequestId) {
        self.machine.mark_pending_action(request, None, true);
    }

    /// Mark a toggle as pending, remembering what its confirmation should
    /// tell the transcript.
    pub(crate) fn mark_pending_toggle(
        &mut self,
        request: RequestId,
        name: String,
        enabled: bool,
        session_only: bool,
    ) {
        self.machine.mark_pending_action(
            request,
            Some(McpToggleReceipt {
                name,
                enabled,
                session_only,
            }),
            false,
        );
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
}

#[cfg(test)]
mod tests;
