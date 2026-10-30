//! /worktrees pane actions: the list refresh on pane-open, cursor movement,
//! and the host-direct verbs. Entering a worktree, reading what a removal
//! would discard, and removing one are answered by the host over the
//! protocol, so none of them costs a model turn.
//!
//! The verbs name the directory the row shows. Whether that directory is a
//! worktree this product created is the host's answer, not something the pane
//! derives from the path.

use crate::agent_message::ClientCommand;
use crate::composition::WorktreeEntry;
use crate::composition::parse_worktrees;
use crate::list_pane_state::filter_by_query;
use crate::state::App;
use crate::ui::BeginOutcome;
use crate::ui::WorktreeLevel;

impl App {
    /// Refresh the worktree list from git worktree list --porcelain against
    /// the session's working directory. Called on /worktrees pane-open and after
    /// an enter or a removal changes where the session works. Pure client
    /// state, so the next render shows the rows. Resets the cursor so it never
    /// points past the new list. No-op when no working directory is set.
    pub fn refresh_worktrees(&mut self) {
        if self.sessions.current.identity.working_dir.is_empty() {
            self.sessions.current.capabilities.worktree.entries.clear();
            self.ui.worktree_pane.list.cursor = 0;
            return;
        }
        let entries = parse_worktrees(
            Some(&self.sessions.current.identity.working_dir),
            &self.sessions.current.identity.working_dir,
        );
        self.ui.worktree_pane.list.clamp(entries.len());
        self.sessions.current.capabilities.worktree.entries = entries;
    }

    /// Move the /worktrees pane cursor up/down. When search is active, the
    /// cursor jumps to the next/prev item in the filtered subset (skipping
    /// filtered-out items) so Up/Down never lands on an invisible row. The
    /// cursor is always an index into the session's worktree rows, so the
    /// actions act on the visible item the cursor points at.
    pub fn move_worktree_cursor(&mut self, delta: i32) {
        let entries = &self.sessions.current.capabilities.worktree.entries;
        if !self.ui.worktree_pane.list.searching() {
            self.ui.worktree_pane.list.move_cursor(delta, entries.len());
            return;
        }
        let filtered = filter_by_query(
            entries,
            &self.ui.worktree_pane.list.query,
            |e: &WorktreeEntry| vec![&e.path, &e.branch],
        );
        if filtered.is_empty() {
            return;
        }
        let cur = self
            .ui
            .worktree_pane
            .list
            .cursor
            .min(entries.len().saturating_sub(1));
        let pos = filtered.iter().position(|&i| i == cur).unwrap_or(0) as i32;
        let next = (pos + delta).clamp(0, (filtered.len() - 1) as i32) as usize;
        self.ui.worktree_pane.list.cursor = filtered[next];
    }

    /// The worktree directory the pane is looking at: the one the detail or
    /// the confirmation names, or, on the list, the row under the cursor.
    fn viewed_worktree_path(&self) -> Option<String> {
        if self.ui.worktree_pane.level.get() != WorktreeLevel::List
            && let Some(path) = self.ui.worktree_pane.draft.borrow().path()
        {
            return Some(path.to_string());
        }
        self.worktree_at_cursor().map(|entry| entry.path.clone())
    }

    /// The worktree row the cursor points at.
    fn worktree_at_cursor(&self) -> Option<&WorktreeEntry> {
        self.sessions
            .current
            .capabilities
            .worktree
            .entries
            .get(self.ui.worktree_pane.list.cursor)
    }

    /// The listing row for a worktree directory, when the listing still holds
    /// it. A tree that stopped being listed can still be on screen in a draft,
    /// so a missing row means no known standing rather than a removable tree.
    fn worktree_entry(&self, path: &str) -> Option<&WorktreeEntry> {
        self.sessions
            .current
            .capabilities
            .worktree
            .entries
            .iter()
            .find(|entry| entry.path == path)
    }

    /// Say something on the level the user is looking at. The list keeps its
    /// notice on the pane and the other levels keep theirs on the draft, so a
    /// refusal has to be placed where it will be drawn.
    fn worktree_notice(&mut self, notice: String) {
        if self.ui.worktree_pane.level.get() == WorktreeLevel::List {
            self.ui.worktree_pane.set_notice(notice);
        } else {
            self.ui.worktree_pane.draft.borrow_mut().set_notice(notice);
        }
    }

    /// Open the detail view for the worktree under the cursor. The pane shows
    /// the fields the listing already holds and asks the host what a removal
    /// would discard, which the detail shows once it lands. The main working
    /// tree is not a worktree the host can measure for removal, so entering
    /// its detail sends no read: the answer would only be the refusal.
    pub fn open_worktree_detail(&mut self) {
        let Some(entry) = self.worktree_at_cursor() else {
            return;
        };
        let path = entry.path.clone();
        let is_main = entry.is_main;
        let outcome =
            self.ui
                .worktree_pane
                .draft
                .borrow_mut()
                .begin(&path, WorktreeLevel::List, None);
        match outcome {
            BeginOutcome::Busy => self.removal_already_out(),
            BeginOutcome::Ready => self.ui.worktree_pane.set_level(WorktreeLevel::Detail),
            BeginOutcome::Reading => {
                self.ui.worktree_pane.set_level(WorktreeLevel::Detail);
                if !is_main {
                    self.start_preview_read(&path);
                }
            }
        }
    }

    /// Open the removal confirmation for the worktree the pane is looking at.
    ///
    /// The confirmation opens only once the host has answered what a removal
    /// would discard, so a tree that cannot be removed never reaches a page
    /// whose only possible answer is a refusal: this level says why instead.
    /// The main working tree and the tree the session is working in are both
    /// refused here, whichever level the gesture came from, so no level can
    /// send a request the host is certain to reject. The confirmation backs
    /// out to the level it was opened from.
    pub fn begin_worktree_removal(&mut self) {
        let from = self.ui.worktree_pane.level.get();
        let Some(path) = self.viewed_worktree_path() else {
            return;
        };
        let refusal = self.worktree_entry(&path).and_then(|entry| {
            if entry.is_main {
                Some(format!(
                    "{} is the repository's main working tree; it cannot be removed from here",
                    entry.path
                ))
            } else if entry.is_current {
                Some(format!(
                    "{} is the worktree this session works in; enter another one before removing it",
                    entry.path
                ))
            } else {
                None
            }
        });
        if let Some(refusal) = refusal {
            self.worktree_notice(refusal);
            return;
        }
        let outcome = self.ui.worktree_pane.draft.borrow_mut().begin(
            &path,
            from,
            Some(WorktreeLevel::Confirm),
        );
        match outcome {
            BeginOutcome::Busy => self.removal_already_out(),
            BeginOutcome::Ready => self.ui.worktree_pane.set_level(WorktreeLevel::Confirm),
            BeginOutcome::Reading => self.start_preview_read(&path),
        }
    }

    /// Say where a gesture that could not start went instead of dropping it.
    fn removal_already_out(&mut self) {
        self.system_line(
            "worktree: a removal is still with the host; its answer comes first".to_string(),
        );
    }

    /// Ask the host what a removal would discard. The wait is drawn by the
    /// detail page itself, which keeps its field rows in place while the
    /// answer is out; nothing goes into the notice slot, which is reserved
    /// for a refusal the user has to read.
    fn start_preview_read(&mut self, path: &str) {
        if self
            .ui
            .worktree_pane
            .draft
            .borrow()
            .preview_request()
            .is_none()
        {
            self.request_worktree_preview(path);
        }
    }

    /// Ask the host what removing one worktree would discard. Nothing is
    /// written; the answer fills the detail and the confirmation.
    pub fn request_worktree_preview(&mut self, path: &str) {
        let Some(req_id) = self.claim_request_id() else {
            return;
        };
        self.ui
            .worktree_pane
            .draft
            .borrow_mut()
            .expect_preview(req_id);
        self.enqueue_refresh(ClientCommand::WorktreePreviewQuery {
            req_id,
            path: path.to_string(),
        });
    }

    /// Read the worktree again, so a confirmation whose answer went stale
    /// shows what the tree holds now. A removal already with the host is not
    /// disturbed: its answer comes first, and a read would only blank the
    /// page that states where the removal stands.
    pub fn retry_worktree_preview(&mut self) {
        if self.ui.worktree_pane.draft.borrow().removing() {
            self.removal_already_out();
            return;
        }
        let Some(path) = self.viewed_worktree_path() else {
            return;
        };
        self.ui.worktree_pane.draft.borrow_mut().forget_preview();
        self.request_worktree_preview(&path);
    }

    /// Move the session into the worktree the pane is looking at. The host
    /// takes the tree under its lease and answers where the session now works;
    /// a tree another process holds, or one this product did not create, is
    /// refused there. A second press while the first is with the host sends
    /// nothing, so one gesture costs one request.
    pub fn enter_worktree_at_cursor(&mut self) {
        let Some(path) = self.viewed_worktree_path() else {
            return;
        };
        // Entering the tree the session already works in would ask the host
        // for a move to where it already is, and the host answers that with an
        // error. The pane knows the answer, so it says it here.
        if self
            .worktree_entry(&path)
            .is_some_and(|entry| entry.is_current)
        {
            self.worktree_notice(format!(
                "{path} is already the worktree this session works in"
            ));
            return;
        }
        // The host refuses to enter the main working tree, so the pane says so
        // here rather than sending a request whose answer is certain.
        if self
            .worktree_entry(&path)
            .is_some_and(|entry| entry.is_main)
        {
            self.worktree_notice(format!(
                "{path} is the main working tree; enter a linked worktree instead"
            ));
            return;
        }
        if self
            .ui
            .worktree_pane
            .draft
            .borrow()
            .enter_request()
            .is_some()
        {
            return;
        }
        let Some(req_id) = self.claim_request_id() else {
            return;
        };
        self.ui
            .worktree_pane
            .draft
            .borrow_mut()
            .expect_enter(req_id);
        self.enqueue_refresh(ClientCommand::WorktreeEnterQuery { req_id, path });
    }

    /// Submit the removal the confirmation holds. The host re-reads the tree
    /// under the lease it takes and removes nothing unless its state still
    /// matches the preview, so the token on its own removes nothing.
    pub fn confirm_worktree_removal(&mut self) {
        let Some(req_id) = self.claim_request_id() else {
            return;
        };
        let Some(submission) = self
            .ui
            .worktree_pane
            .draft
            .borrow_mut()
            .take_submission(req_id)
        else {
            return;
        };
        self.enqueue_refresh(ClientCommand::WorktreeRemoveQuery {
            req_id,
            path: submission.path,
            token: submission.token,
            delete_unmerged_branch: submission.delete_unmerged_branch,
        });
    }

    /// Flip whether an unmerged branch goes with the tree. Keeping it is the
    /// default, so a removal that discards commits takes two deliberate moves.
    pub fn toggle_worktree_branch_deletion(&mut self) {
        self.ui
            .worktree_pane
            .draft
            .borrow_mut()
            .toggle_branch_deletion();
    }

    /// Back out of the confirmation to the level it was opened from. Nothing
    /// is sent; the preview stays held, so reopening the confirmation does not
    /// read the tree again.
    pub fn close_worktree_removal(&mut self) {
        let back = self.ui.worktree_pane.draft.borrow().return_level();
        self.ui.worktree_pane.set_level(back);
    }
}

#[cfg(test)]
mod tests;
