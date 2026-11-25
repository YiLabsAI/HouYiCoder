//! /mcp pane actions: reconnect and the enable toggle.
//!
//! Both resolve through the pane's own cursor, so the row the user sees
//! selected is the row acted on. The pane refuses a key its selected row
//! does not support before anything is sent, and each action remembers the
//! request its reply must match, so a late or stale reply changes nothing.

use std::time::Instant;

use houyicoder_protocol::mcp::{McpConfigScope, McpConnectionState};

use crate::agent_message::ClientCommand;
use crate::state::App;
use crate::state::enums::Pane;

impl App {
    /// Ask for a fresh status list while a row the host reported is still
    /// settling (starting, reconnecting). The pane's rows refresh only on a
    /// reply, and a handshake or relaunch finishes later with nothing
    /// pushing it back, so the loop calls this until the rows settle or the
    /// action's asking budget runs out. True when a query was sent, so the
    /// caller redraws.
    pub(crate) fn tick_mcp_refresh(&mut self, now: Instant) -> bool {
        if self.ui.navigation.pane != Pane::Mcp {
            return false;
        }
        if !self.ui.mcp_pane.take_due_refresh(now) {
            return false;
        }
        if self.sessions.current.connection.is_none() {
            return false;
        }
        let Some(req_id) = self.claim_request_id() else {
            return false;
        };
        self.ui.mcp_pane.mark_settling_pending(req_id);
        self.enqueue_refresh(ClientCommand::McpStatusQuery { req_id });
        true
    }
    /// Ask the server under the cursor for a fresh handshake. A disabled
    /// server has nothing to reconnect, so the key never reaches here.
    pub(crate) fn reconnect_mcp_at_cursor(&mut self) {
        let Some(name) = self.ui.mcp_pane.selected().map(|row| row.name.clone()) else {
            return;
        };
        if let Some(req_id) = self.claim_request_id() {
            self.ui.mcp_pane.mark_pending_reconnect(req_id);
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
        // a disable request: the key flips the state the row shows. Only a
        // user-scope entry persists its answer, so the receipt of any other
        // scope says the choice lasts this session.
        let enabled = matches!(row.state, McpConnectionState::Disabled { .. });
        let session_only = row.scope != McpConfigScope::User;
        if let Some(req_id) = self.claim_request_id() {
            self.ui
                .mcp_pane
                .mark_pending_toggle(req_id, name.clone(), enabled, session_only);
            self.enqueue_refresh(ClientCommand::McpSetEnabledQuery {
                req_id,
                name,
                enabled,
            });
        }
    }
}
