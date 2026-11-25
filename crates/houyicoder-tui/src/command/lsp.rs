//! /lsp pane actions: restart and the enable toggle.
//!
//! Both resolve through the pane's own cursor, so the row the user sees
//! selected is the row acted on. The pane refuses a key its selected row
//! does not support before anything is sent, and each action remembers the
//! request its reply must match, so a late or stale reply changes nothing.

use std::time::Instant;

use houyicoder_protocol::lsp::LspConfigScope;
use houyicoder_protocol::lsp::LspServerState;

use crate::agent_message::ClientCommand;
use crate::state::App;
use crate::state::enums::Pane;

impl App {
    /// Ask for a fresh status list while a row the host reported is still
    /// settling (starting, indexing). The pane's rows refresh only on a
    /// reply, and a handshake or an indexing pass finishes later with
    /// nothing pushing it back, so the loop calls this until the rows
    /// settle or the action's asking budget runs out. True when a query was
    /// sent, so the caller redraws.
    pub(crate) fn tick_lsp_refresh(&mut self, now: Instant) -> bool {
        if self.ui.navigation.pane != Pane::Lsp {
            return false;
        }
        if !self.ui.lsp_pane.take_due_refresh(now) {
            return false;
        }
        if self.sessions.current.connection.is_none() {
            return false;
        }
        let Some(req_id) = self.claim_request_id() else {
            return false;
        };
        self.ui.lsp_pane.mark_settling_pending(req_id);
        self.enqueue_refresh(ClientCommand::LspStatusQuery { req_id });
        true
    }

    /// Ask the server under the cursor to stop and start again. A disabled
    /// server has nothing to restart, so the key never reaches here.
    pub(crate) fn restart_lsp_at_cursor(&mut self) {
        let Some(name) = self.ui.lsp_pane.selected().map(|row| row.name.clone()) else {
            return;
        };
        if let Some(req_id) = self.claim_request_id() {
            self.ui.lsp_pane.mark_pending_restart(req_id);
            self.enqueue_refresh(ClientCommand::LspRestartQuery { req_id, name });
        }
    }

    /// Flip the server under the cursor between enabled and disabled. A
    /// server the trust gate holds is not toggleable, so the key never
    /// reaches here. The receipt lands when the host's reply confirms the
    /// new state, not when the key was pressed.
    pub(crate) fn toggle_lsp_at_cursor(&mut self) {
        let Some(row) = self.ui.lsp_pane.selected() else {
            return;
        };
        let name = row.name.clone();
        // A disabled row's toggle is an enable request, and a live row's is
        // a disable request: the key flips the state the row shows. Only a
        // user-scope entry persists its answer, so the receipt of any other
        // scope says the choice lasts this session.
        let enabled = matches!(row.state, LspServerState::Disabled { .. });
        let session_only = row.scope != LspConfigScope::User;
        if let Some(req_id) = self.claim_request_id() {
            self.ui
                .lsp_pane
                .mark_pending_toggle(req_id, name.clone(), enabled, session_only);
            self.enqueue_refresh(ClientCommand::LspSetEnabledQuery {
                req_id,
                name,
                enabled,
            });
        }
    }
}
