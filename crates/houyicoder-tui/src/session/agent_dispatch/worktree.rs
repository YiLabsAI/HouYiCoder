//! Applies the host's answers to the /worktrees pane verbs: the removal
//! preview, the enter, and how a confirmed removal ended. Every answer is
//! matched to the request the pane sent. An enter or a removal the user has
//! walked away from still reaches the transcript, because the host changed
//! something; a preview reaches only the screen that asked for it, since a
//! read on its own changed nothing.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::worktree::{
    WorktreeEnterView, WorktreePreviewView, WorktreeRemoveOutcome,
};

use crate::state::App;
use crate::ui::WorktreeLevel;

impl App {
    /// Show a preview the host measured, when the pane still waits on that
    /// request and on that worktree.
    pub(crate) fn apply_worktree_preview(
        &mut self,
        request: RequestId,
        preview: WorktreePreviewView,
    ) {
        let mut draft = self.ui.worktree_pane.draft.borrow_mut();
        if draft.preview_request() != Some(request) {
            return;
        }
        draft.finish_preview();
        draft.accept(preview);
    }

    /// The session moved into a worktree. Wherever the pane was, the
    /// transcript records the move and the list is read again, so the row the
    /// session now works in is the current one.
    pub(crate) fn apply_worktree_entered(
        &mut self,
        request: RequestId,
        entered: WorktreeEnterView,
    ) {
        let waited = {
            let mut draft = self.ui.worktree_pane.draft.borrow_mut();
            if draft.enter_request() != Some(request) {
                false
            } else {
                draft.finish_enter();
                if !draft.removing() {
                    draft.clear();
                }
                true
            }
        };
        self.system_line(format!(
            "worktree: now working in {} ({})",
            entered.path, entered.branch
        ));
        self.sessions.current.identity.working_dir = entered.path;
        if waited {
            self.ui.worktree_pane.set_level(WorktreeLevel::List);
        }
        self.refresh_worktrees();
    }

    /// Show how a confirmed removal ended. The answer belongs to the removal
    /// that sent it: when the pane has moved on, the transcript records what
    /// the host did and the list is read again, rather than the answer landing
    /// on a screen that never asked for it.
    ///
    /// A stale token means the tree changed since the preview. Nothing was
    /// removed, the confirmation takes a submission again, and the tree is
    /// read once more so the next Enter decides on what it holds now.
    pub(crate) fn apply_worktree_removed(
        &mut self,
        request: RequestId,
        outcome: WorktreeRemoveOutcome,
    ) {
        let waited = {
            let mut draft = self.ui.worktree_pane.draft.borrow_mut();
            if draft.request() != Some(request) {
                false
            } else {
                draft.finish_removal();
                true
            }
        };
        let sentence = removal_sentence(&outcome);
        if !waited {
            self.system_line(sentence);
            if removal_changed_the_list(&outcome) {
                self.refresh_worktrees();
            }
            return;
        }
        match outcome {
            WorktreeRemoveOutcome::Removed => {
                self.ui.worktree_pane.draft.borrow_mut().clear();
                self.ui.worktree_pane.set_level(WorktreeLevel::List);
                self.system_line(sentence);
                self.refresh_worktrees();
            }
            WorktreeRemoveOutcome::Stale => {
                self.ui
                    .worktree_pane
                    .draft
                    .borrow_mut()
                    .set_notice(sentence);
                self.retry_worktree_preview();
            }
            // Both unfinished shapes leave a record and take the registration
            // apart, so the list on screen no longer describes the repository.
            WorktreeRemoveOutcome::Incomplete { .. } | WorktreeRemoveOutcome::Pending { .. } => {
                {
                    let mut draft = self.ui.worktree_pane.draft.borrow_mut();
                    draft.forget_preview();
                    draft.set_notice(sentence);
                }
                self.refresh_worktrees();
            }
            _ => {
                let mut draft = self.ui.worktree_pane.draft.borrow_mut();
                draft.forget_preview();
                draft.set_notice(sentence);
            }
        }
    }

    /// Settle a worktree request the host refused. Only the request the pane
    /// is waiting on is settled; an answer to an older one leaves the pane as
    /// it is and is reported as unhandled.
    pub(crate) fn settle_worktree_failure(&mut self, request: RequestId, message: &str) -> bool {
        let mut entered = false;
        {
            let mut draft = self.ui.worktree_pane.draft.borrow_mut();
            if draft.request() == Some(request) {
                draft.finish_removal();
                draft.set_notice(format!("removal refused: {message}"));
                return true;
            }
            if draft.preview_request() == Some(request) {
                draft.finish_preview();
                draft.set_notice(format!("cannot read the worktree: {message}"));
                return true;
            }
            if draft.enter_request() == Some(request) {
                draft.finish_enter();
                entered = true;
            }
        }
        if !entered {
            return false;
        }
        self.system_line(format!("worktree: cannot enter: {message}"));
        true
    }
}

/// What a removal answer says, wherever it is shown.
fn removal_sentence(outcome: &WorktreeRemoveOutcome) -> String {
    match outcome {
        WorktreeRemoveOutcome::Removed => "worktree: removed".to_string(),
        WorktreeRemoveOutcome::Stale => {
            "the worktree changed since the preview; nothing was removed".to_string()
        }
        WorktreeRemoveOutcome::Incomplete { branch, reason, .. } => {
            format!("the directory is gone; branch {branch} survives: {reason}")
        }
        WorktreeRemoveOutcome::Pending { reason, .. } => {
            format!("the directory survives: {reason}")
        }
        // Every answer this version knows is above, so this is a state a later
        // host named: say that rather than printing the variant to the user.
        _ => "the removal ended in a state this version does not know".to_string(),
    }
}

/// Whether an answer leaves the list on screen out of date.
fn removal_changed_the_list(outcome: &WorktreeRemoveOutcome) -> bool {
    matches!(
        outcome,
        WorktreeRemoveOutcome::Removed
            | WorktreeRemoveOutcome::Incomplete { .. }
            | WorktreeRemoveOutcome::Pending { .. }
    )
}
