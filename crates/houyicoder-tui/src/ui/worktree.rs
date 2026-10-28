//! The /worktrees pane state: cursor, search query, drill-down level, and the
//! removal confirmation. Every reply is matched to the request the pane sent.
//! A preview answer the pane no longer waits on is dropped, since a read on
//! its own changed nothing; an enter or a removal answer still reaches the
//! transcript, because the host changed something.

use std::cell::{Cell, RefCell};

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::WorktreePreviewView;

use crate::view::list_pane_state::ListPaneState;

/// How far into a worktree the pane has drilled down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorktreeLevel {
    /// The list of worktrees.
    #[default]
    List,
    /// One worktree's detail.
    Detail,
    /// The confirmation that removes one worktree.
    Confirm,
}

/// Which button the removal confirmation holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RemoveFocus {
    /// Back out without removing anything. The row starts here, so removing
    /// takes one deliberate move.
    #[default]
    Cancel,
    /// Remove the worktree the preview describes.
    Remove,
}

/// What pointing the draft at a worktree found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginOutcome {
    /// The draft shows this worktree and holds its preview.
    Ready,
    /// The draft shows this worktree and waits on its preview.
    Reading,
    /// A removal for another worktree is with the host, so the draft stays
    /// pointed at it until that answer lands.
    Busy,
}

/// The list cursor, the drill-down level, and the removal confirmation.
#[derive(Default)]
pub struct WorktreePaneState {
    /// The cursor and the search query.
    pub list: ListPaneState,
    /// How far into a worktree the pane has drilled down.
    pub level: Cell<WorktreeLevel>,
    /// Whether the list takes keystrokes as a search query.
    pub search: Cell<bool>,
    /// How many rows the detail or the confirmation is scrolled into its
    /// block. A block that fits reads as zero; the renderer holds it at the
    /// last row that still fills the block.
    pub scroll: Cell<u16>,
    /// The removal confirmation the pane is holding.
    pub draft: RefCell<RemovalDraft>,
}

impl WorktreePaneState {
    /// Leave whatever the pane was holding and start again at the list.
    pub fn reset(&mut self) {
        self.list.clear_query();
        self.set_level(WorktreeLevel::List);
        self.search.set(false);
        self.draft.borrow_mut().clear();
    }

    /// Move to another level, which starts that level's block at its top: a
    /// scroll position belongs to the block it was made in.
    pub fn set_level(&self, level: WorktreeLevel) {
        self.level.set(level);
        self.scroll.set(0);
    }
}

/// What a confirmed removal asks the host to do: the directory the row names,
/// whether the unmerged branch goes with it, and the token itself.
#[derive(Debug)]
pub struct RemovalSubmission {
    /// The worktree directory to remove.
    pub path: String,
    /// The token the preview answered with.
    pub token: String,
    /// Whether an unmerged branch goes with the tree.
    pub delete_unmerged_branch: bool,
}

/// The worktree the pane acts on, the preview the host measured for it, which
/// button the row holds, and the answers the host has given.
#[derive(Default)]
pub struct RemovalDraft {
    path: Option<String>,
    preview: Option<WorktreePreviewView>,
    preview_request: Option<RequestId>,
    focus: RemoveFocus,
    request: Option<RequestId>,
    enter_request: Option<RequestId>,
    delete_unmerged_branch: bool,
    return_level: WorktreeLevel,
    notice: Option<String>,
}

impl RemovalDraft {
    /// Point the draft at a worktree and name the level it returns to. A
    /// preview already held for the same worktree is kept, and so is a read
    /// already on its way, so moving between the levels does not read the
    /// tree twice.
    pub fn begin(&mut self, path: &str, return_level: WorktreeLevel) -> BeginOutcome {
        if self.path.as_deref() != Some(path) {
            if self.request.is_some() {
                return BeginOutcome::Busy;
            }
            self.clear();
            self.path = Some(path.to_string());
        }
        self.return_level = return_level;
        self.focus = RemoveFocus::Cancel;
        self.notice = None;
        if self.preview.is_some() {
            BeginOutcome::Ready
        } else {
            BeginOutcome::Reading
        }
    }

    /// The worktree directory this draft acts on.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Record the request the preview went out under, so a late answer that
    /// names another request is dropped. The last answer stays until the fresh
    /// read lands, so a retry does not blank the reason it was made.
    pub fn expect_preview(&mut self, request: RequestId) {
        self.preview_request = Some(request);
    }

    /// The request the draft waits on for a preview.
    pub fn preview_request(&self) -> Option<RequestId> {
        self.preview_request
    }

    /// The preview read ended, whatever the answer.
    pub fn finish_preview(&mut self) {
        self.preview_request = None;
    }

    /// Record the request an enter went out under, so only its own answer ends
    /// the pane's wait.
    pub fn expect_enter(&mut self, request: RequestId) {
        self.enter_request = Some(request);
    }

    /// The request the draft waits on for an enter.
    pub fn enter_request(&self) -> Option<RequestId> {
        self.enter_request
    }

    /// The enter ended, whatever the answer.
    pub fn finish_enter(&mut self) {
        self.enter_request = None;
    }

    /// Take a preview the host measured. The caller has already matched the
    /// answer to the request this draft sent, and a draft pointed elsewhere
    /// clears that record, so the answer belongs to the tree on screen.
    pub fn accept(&mut self, preview: WorktreePreviewView) {
        self.preview_request = None;
        self.notice = None;
        self.preview = Some(preview);
    }

    /// The preview to show, once the host has answered.
    pub fn preview(&self) -> Option<&WorktreePreviewView> {
        self.preview.as_ref()
    }

    /// Forget the preview, so the confirmation shows a fresh read.
    pub fn forget_preview(&mut self) {
        self.preview = None;
    }

    /// Which button the row holds.
    pub fn focus(&self) -> RemoveFocus {
        self.focus
    }

    /// Move the focus to the other button.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            RemoveFocus::Cancel => RemoveFocus::Remove,
            RemoveFocus::Remove => RemoveFocus::Cancel,
        };
    }

    /// The level the confirmation returns to.
    pub fn return_level(&self) -> WorktreeLevel {
        self.return_level
    }

    /// Whether the removal carries the unmerged branch with it.
    pub fn delete_unmerged_branch(&self) -> bool {
        self.delete_unmerged_branch
    }

    /// Choose whether an unmerged branch goes with the tree.
    pub fn toggle_branch_deletion(&mut self) {
        self.delete_unmerged_branch = !self.delete_unmerged_branch;
    }

    /// Take what a confirmed removal submits, under the request it goes out
    /// as, or None when there is nothing to submit: no preview yet, a worktree
    /// a process still holds, or a removal already with the host. The check and
    /// the request record happen together, so a second Enter cannot send the
    /// same removal twice.
    pub fn take_submission(&mut self, request: RequestId) -> Option<RemovalSubmission> {
        if self.request.is_some() {
            return None;
        }
        let preview = self.preview.as_ref()?;
        if preview.leased || preview.locked {
            return None;
        }
        let submission = RemovalSubmission {
            path: self.path.clone()?,
            token: preview.token.clone(),
            delete_unmerged_branch: self.delete_unmerged_branch,
        };
        self.request = Some(request);
        self.notice = None;
        Some(submission)
    }

    /// Whether a removal for this draft is with the host.
    pub fn removing(&self) -> bool {
        self.request.is_some()
    }

    /// The request the draft waits on.
    pub fn request(&self) -> Option<RequestId> {
        self.request
    }

    /// The removal ended, whatever the answer: a retry is possible again.
    pub fn finish_removal(&mut self) {
        self.request = None;
    }

    /// The host's last answer, shown above the buttons.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Record the host's answer.
    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// Forget everything: the worktree, its preview, and the answers.
    pub fn clear(&mut self) {
        self.path = None;
        self.preview = None;
        self.preview_request = None;
        self.focus = RemoveFocus::Cancel;
        self.request = None;
        self.enter_request = None;
        self.delete_unmerged_branch = false;
        self.return_level = WorktreeLevel::List;
        self.notice = None;
    }
}

#[cfg(test)]
mod tests;
