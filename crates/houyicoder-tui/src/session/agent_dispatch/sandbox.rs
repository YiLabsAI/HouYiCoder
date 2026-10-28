//! Applies the host's answers about the sandbox: what confines the session,
//! which writes it recorded, and what a restore did. A read reaches only the
//! tab that asked for it, since a read on its own changed nothing; a restore
//! reaches the transcript whatever the pane is showing, because the host
//! changed a file.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointListView, CheckpointRestoreView, SandboxBoundariesView,
};

use crate::state::App;
use crate::ui::SandboxTab;
use crate::view::sandbox_pane::restore_sentence;

impl App {
    /// Show the boundaries the host measured, when the pane still waits on that
    /// request.
    pub(crate) fn apply_sandbox_boundaries(
        &mut self,
        request: RequestId,
        view: SandboxBoundariesView,
    ) {
        if self.ui.sandbox_pane.boundaries_request() != Some(request) {
            return;
        }
        self.ui.sandbox_pane.accept_boundaries(view);
    }

    /// Show the listing the host judged, when the pane still waits on that
    /// request.
    pub(crate) fn apply_sandbox_checkpoints(
        &mut self,
        request: RequestId,
        view: CheckpointListView,
    ) {
        if self.ui.sandbox_pane.list_request() != Some(request) {
            return;
        }
        self.ui.sandbox_pane.accept_checkpoints(view);
    }

    /// Show what a restore did. The answer always reaches the transcript,
    /// whether or not the pane still waits on it, because a file changed and
    /// the sentence has to outlive the pane. The confirmation then closes back
    /// to the level it was opened from, and the listing is read again so the
    /// row states what the target holds now.
    pub(crate) fn apply_sandbox_restored(
        &mut self,
        request: RequestId,
        view: CheckpointRestoreView,
    ) {
        let sentence = restore_sentence(&view.operation, &view.outcome);
        self.system_line(sentence);
        if self.ui.sandbox_pane.restore_request() != Some(request) {
            return;
        }
        self.ui.sandbox_pane.end_restore();
        let back = self.ui.sandbox_pane.restore_from();
        self.ui.sandbox_pane.set_level(back);
        if self.ui.sandbox_pane.tab.get() == SandboxTab::Checkpoints {
            self.refresh_sandbox_tab();
        }
    }

    /// Settle a sandbox request the host refused. Only a request the pane waits
    /// on is settled; an answer to an older one leaves the pane as it is and is
    /// reported as unhandled. A refused restore also reaches the transcript,
    /// since leaving the confirmation would otherwise drop the reason.
    pub(crate) fn settle_sandbox_failure(&mut self, request: RequestId, message: &str) -> bool {
        let pane = &self.ui.sandbox_pane;
        if pane.restore_request() == Some(request) {
            let sentence = format!("restore refused: {message}");
            pane.finish_restore(sentence.clone());
            self.system_line(sentence);
            return true;
        }
        if pane.list_request() == Some(request) {
            pane.abandon_list();
            pane.set_notice(format!("cannot list the checkpoints: {message}"));
            return true;
        }
        if pane.boundaries_request() == Some(request) {
            pane.abandon_boundaries();
            pane.set_notice(format!("cannot read the boundaries: {message}"));
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests;
