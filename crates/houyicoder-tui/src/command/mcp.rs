//! /mcp pane actions: reconnect and the enable toggle.
//!
//! Both resolve through the pane's own cursor, so the row the user sees
//! selected is the row acted on. The pane refuses a key its selected row
//! does not support before anything is sent, and each action remembers the
//! request its reply must match, so a late or stale reply changes nothing.

use houyicoder_protocol::mcp::McpConnectionState;

use crate::agent_message::ClientCommand;
use crate::state::App;

impl App {
    /// Ask the server under the cursor for a fresh handshake. A disabled
    /// server has nothing to reconnect, so the key never reaches here.
    pub(crate) fn reconnect_mcp_at_cursor(&mut self) {
        let Some(name) = self.ui.mcp_pane.selected().map(|row| row.name.clone()) else {
            return;
        };
        if let Some(req_id) = self.claim_request_id() {
            self.ui.mcp_pane.mark_pending(req_id);
            self.enqueue_refresh(ClientCommand::McpReconnectQuery { req_id, name });
        }
    }

    /// Flip the server under the cursor between enabled and disabled. A
    /// server an untrusted workspace locked is not toggleable, so the key
    /// never reaches here. The receipt lands when the host's reply confirms
    /// the new state, not when the key was pressed.
    pub(crate) fn toggle_mcp_at_cursor(&mut self) {
        let Some(row) = self.ui.mcp_pane.selected() else {
            return;
        };
        let name = row.name.clone();
        // A disabled row's toggle is an enable request, and a live row's is
        // a disable request: the key flips the state the row shows.
        let enabled = matches!(row.state, McpConnectionState::Disabled { .. });
        if let Some(req_id) = self.claim_request_id() {
            self.ui
                .mcp_pane
                .mark_pending_toggle(req_id, name.clone(), enabled);
            self.enqueue_refresh(ClientCommand::McpSetEnabledQuery {
                req_id,
                name,
                enabled,
            });
        }
    }
}
