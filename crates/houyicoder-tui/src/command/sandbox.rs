//! /sandbox and /rewind pane actions: opening the pane on a tab, reading what
//! the host answers, and the restore the confirmation submits. Every verb is
//! answered by the host over the protocol, so none of them costs a model turn
//! and none of them touches the conversation.

use houyicoder_protocol::frontend::checkpoint::CheckpointState;

use crate::agent_message::ClientCommand;
use crate::state::App;
use crate::state::Pane;
use crate::ui::enums::CyclicTab;
use crate::ui::{CheckpointLevel, SandboxTab};
use crate::view::sandbox_pane::{CheckpointRow, row_at, row_count};

impl App {
    /// Open the pane on a tab and read it from the host. The boundaries answer
    /// what confines the session; the checkpoints answer what the session
    /// wrote. Neither rewinds anything, and neither turns a page of the
    /// conversation.
    pub fn open_sandbox_pane(&mut self, tab: SandboxTab) {
        self.ui.navigation.pane = Pane::Sandbox;
        self.ui.sandbox_pane.set_tab(tab);
        self.refresh_sandbox_tab();
    }

    /// Read the open tab again. A read already with the host stays the one
    /// answer, so one gesture costs one request.
    pub fn refresh_sandbox_tab(&mut self) {
        match self.ui.sandbox_pane.tab.get() {
            SandboxTab::Sandbox => self.request_sandbox_boundaries(),
            SandboxTab::Checkpoints => self.request_sandbox_checkpoints(),
        }
    }

    fn request_sandbox_boundaries(&mut self) {
        if self.ui.sandbox_pane.reading_boundaries() {
            return;
        }
        let Some(request) = self.claim_request_id() else {
            return;
        };
        self.ui.sandbox_pane.expect_boundaries(request);
        self.enqueue_refresh(ClientCommand::SandboxBoundariesQuery { req_id: request });
    }

    fn request_sandbox_checkpoints(&mut self) {
        if self.ui.sandbox_pane.reading_checkpoints() {
            return;
        }
        let Some(request) = self.claim_request_id() else {
            return;
        };
        self.ui.sandbox_pane.expect_list(request);
        self.enqueue_refresh(ClientCommand::SandboxCheckpointsQuery { req_id: request });
    }

    /// Switch to the other tab, in the direction a left or right key asked
    /// for, and read it. A read of that tab already with the host is the one
    /// answer, so a quick return costs no second request.
    pub fn switch_sandbox_tab(&mut self, delta: i32) {
        let current = self.ui.sandbox_pane.tab.get();
        let next = if delta < 0 {
            current.prev()
        } else {
            current.next()
        };
        self.ui.sandbox_pane.set_tab(next);
        self.refresh_sandbox_tab();
    }

    /// Move the listing cursor by this many rows, held inside the rows the
    /// host answered.
    pub fn move_checkpoint_cursor(&mut self, delta: i32) {
        let rows = {
            let listing = self.ui.sandbox_pane.checkpoints();
            listing.as_ref().map(row_count).unwrap_or(0)
        };
        self.ui.sandbox_pane.move_cursor(delta, rows);
    }

    /// Open the detail of the row under the cursor, and read the two sides of
    /// that write so the detail can compare them.
    pub fn open_checkpoint_detail(&mut self) {
        if !self.selected_row_exists() {
            return;
        }
        self.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
        self.request_checkpoint_texts();
    }

    /// Read both sides of the row under the cursor. A read already with the
    /// host stays the one answer, and a row the host could not judge has no
    /// sides to read: its detail is the reason it could not be read.
    pub fn request_checkpoint_texts(&mut self) {
        if self.ui.sandbox_pane.reading_texts() {
            return;
        }
        let Some((operation, _, _)) = self.selected_write() else {
            return;
        };
        let Some(request) = self.claim_request_id() else {
            return;
        };
        self.ui.sandbox_pane.expect_texts(request);
        self.enqueue_refresh(ClientCommand::SandboxTextsQuery {
            req_id: request,
            operation,
        });
    }

    /// Read both sides again, so a detail whose target changed since the read
    /// shows what it holds now.
    pub fn reread_checkpoint_texts(&mut self) {
        self.ui.sandbox_pane.forget_texts();
        self.request_checkpoint_texts();
    }

    /// Open the restore confirmation for the row under the cursor.
    pub fn begin_checkpoint_restore(&mut self) {
        if self.restorable_operation().is_none() {
            return;
        }
        self.ui.sandbox_pane.begin_confirm();
    }

    /// Submit the restore the confirmation holds. The host re-reads the target
    /// under the lock the write path takes, so a target that changed since the
    /// write keeps its bytes and the earlier ones go to a copy beside it.
    pub fn confirm_checkpoint_restore(&mut self) {
        if self.ui.sandbox_pane.restore_request().is_some() {
            return;
        }
        let Some(operation) = self.restorable_operation() else {
            return;
        };
        let Some(request) = self.claim_request_id() else {
            return;
        };
        if !self.ui.sandbox_pane.begin_restore(request) {
            return;
        }
        self.enqueue_refresh(ClientCommand::SandboxRestoreQuery {
            req_id: request,
            operation,
        });
    }

    /// Back out of the confirmation to the level it was opened from. Nothing
    /// is sent, so a restore the user reconsidered costs the host nothing.
    pub fn close_checkpoint_restore(&mut self) {
        let back = self.ui.sandbox_pane.restore_from();
        self.ui.sandbox_pane.set_level(back);
        // Entering the restore drops the comparison the detail had read, so a
        // cancel that returns to the detail has to ask for it again; otherwise
        // the page the user comes back to has lost both sides.
        if back == CheckpointLevel::Detail {
            self.request_checkpoint_texts();
        }
    }

    /// Esc: return one level, and close the pane only at a tab's root.
    pub fn sandbox_back(&mut self) {
        match self.ui.sandbox_pane.tab.get() {
            SandboxTab::Sandbox => self.close_sandbox_pane(),
            SandboxTab::Checkpoints => match self.ui.sandbox_pane.level.get() {
                CheckpointLevel::Restore => self.close_checkpoint_restore(),
                CheckpointLevel::Detail => self.ui.sandbox_pane.set_level(CheckpointLevel::List),
                CheckpointLevel::List => self.close_sandbox_pane(),
            },
        }
    }

    fn close_sandbox_pane(&mut self) {
        self.ui.sandbox_pane.reset();
        self.ui.navigation.pane = Pane::Transcript;
    }

    /// Whether the listing holds a row under the cursor. A record the host
    /// could not judge still opens its detail: the reason it could not be read
    /// is the detail.
    fn selected_row_exists(&self) -> bool {
        let listing = self.ui.sandbox_pane.checkpoints();
        let cursor = self.ui.sandbox_pane.cursor.get();
        listing
            .as_ref()
            .is_some_and(|l| row_at(l, cursor).is_some())
    }

    /// The operation of the row under the cursor, when there are earlier bytes
    /// to put back. A row the host could not judge, and one whose earlier
    /// bytes were never kept, are named in the pane instead: pressing the key
    /// then says why it did nothing rather than opening buttons that cannot
    /// act.
    fn restorable_operation(&mut self) -> Option<String> {
        let row = {
            let listing = self.ui.sandbox_pane.checkpoints();
            let cursor = self.ui.sandbox_pane.cursor.get();
            listing
                .as_ref()
                .and_then(|list| row_at(list, cursor))
                .map(|row| match row {
                    CheckpointRow::Unreadable(view) => Err(view.reason.clone()),
                    CheckpointRow::Write(view) => {
                        Ok((view.operation.clone(), view.target.clone(), view.state))
                    }
                })?
        };
        let (operation, target, state) = match row {
            Ok(fields) => fields,
            Err(reason) => {
                self.ui.sandbox_pane.set_notice(format!(
                    "checkpoint: the host could not judge this record, so there is \
                     nothing to put back: {reason}"
                ));
                return None;
            }
        };
        if state == CheckpointState::Unverified {
            self.ui.sandbox_pane.set_notice(format!(
                "checkpoint: the earlier bytes of {target} were never kept; there is nothing to put back"
            ));
            return None;
        }
        Some(operation)
    }

    /// The operation, target, and state of the row under the cursor, when it
    /// is a write the host judged. The target is what the pane names back to
    /// the user; the operation is the name the host restores it under.
    fn selected_write(&self) -> Option<(String, String, CheckpointState)> {
        let listing = self.ui.sandbox_pane.checkpoints();
        let cursor = self.ui.sandbox_pane.cursor.get();
        match row_at(listing.as_ref()?, cursor)? {
            CheckpointRow::Write(view) => {
                Some((view.operation.clone(), view.target.clone(), view.state))
            }
            CheckpointRow::Unreadable(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
