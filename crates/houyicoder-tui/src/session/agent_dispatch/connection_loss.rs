//! What a dead transport strands, reported on the session that owned it.
//!
//! Every request whose transport is gone can never be answered, so each
//! pending operation is either ended or handed back to the user here: the run
//! completes with the loss, the direct shell command reports its interruption,
//! and the confirmation pages release the keys they were holding.

use houyicoder_protocol::envelope::RequestId;

use crate::pending_prompt::PendingPrompt;
use crate::state::App;

impl App {
    /// Settle a connection loss, returning whether this call performed the
    /// non-Lost → Lost cleanup. The first-cause-wins transition gates the
    /// work, so an announced death followed by a closed channel (or a repeat
    /// closed observation) neither overwrites the cause nor repeats the
    /// completion. A session-less App (the no-backend path) has no cause to
    /// record but still sweeps that observation.
    ///
    /// not_sent lists request ids the driver can prove it never attempted;
    /// the active run and memory actions whose ids appear in it are not sent
    /// (never left this process). Every other pending request is
    /// conservatively unknown — a write or flush may have delivered the frame.
    pub(crate) fn apply_connection_loss(
        &mut self,
        cause: String,
        not_sent: Vec<RequestId>,
    ) -> bool {
        let first = match self.sessions.current.connection.as_mut() {
            Some(s) => s.mark_lost(cause.clone()),
            // No connection: nothing was lost twice, but run the sweep anyway.
            // A session-less App (tests, the stub path) still clears run and pane state.
            None => true,
        };
        if !first {
            return false;
        }
        // Distinguish not sent (never attempted) from unknown (a flush may
        // have partially delivered); the run line tells the user which.
        let run_req = self.active_run_req_id();
        let run_not_sent = run_req.is_some_and(|r| not_sent.contains(&r));
        self.clear_memory_pending();
        self.ui.skills_pane.clear_pending();
        // A request whose transport is gone can never be answered, and both
        // confirmation pages ignore every key while one is out. Settling them
        // is what returns the keys: without it the page has no button, no key,
        // and no answer coming, so the only way out is abandoning the session.
        self.settle_pending_worktree_removal();
        self.settle_pending_restore();
        // A direct shell command cannot answer a dead transport either. Its
        // end is reported here rather than waiting on a reply that will never
        // arrive, so the status bar stops advertising a command that is gone.
        let shell_req = self.sessions.current.shell.request_id();
        if let Some(shell_req) = shell_req {
            self.sessions.current.shell.finish();
            self.system_line(if not_sent.contains(&shell_req) {
                "shell command not sent — the connection failed before it reached the transport"
                    .to_string()
            } else {
                format!("shell command interrupted: {cause}")
            });
        }
        let run_line = if run_not_sent {
            "run not sent — connection failed before the request reached the transport"
        } else {
            cause.as_str()
        };
        if run_req.is_some() {
            self.handle_run_completion(Err(run_line.to_string()));
        } else {
            // A pending trust verdict can never reach a dead transport. Clear
            // it locally (no fake decline), report the project and cause, and
            // release the exit keys the card was swallowing. Other prompts
            // return to the field so a later settle still owns them.
            let trust_path = match self.sessions.current.prompt.as_ref() {
                Some(PendingPrompt::Trust { prompt: tp, .. }) => Some(tp.project_path.clone()),
                _ => None,
            };
            if let Some(path) = trust_path {
                self.sessions.current.clear_prompt();
                self.system_line(format!("trust verdict undelivered for {path}: {run_line}"));
            } else if shell_req.is_none() {
                // When a direct shell command was the only pending work, the
                // branch above already reported the loss for the operation
                // that owned it. Repeating the cause as an agent error would
                // blame a run that never existed.
                self.system_line(format!("agent error: {run_line}"));
            }
        }
        true
    }

    /// Release a worktree page that is waiting on the host for an answer the
    /// dead transport can never carry.
    fn settle_pending_worktree_removal(&mut self) {
        let mut draft = self.ui.worktree_pane.draft.borrow_mut();
        draft.finish_removal();
        draft.finish_preview();
        draft.finish_enter();
    }

    /// The same release for the sandbox page's restore confirmation.
    fn settle_pending_restore(&mut self) {
        self.ui.sandbox_pane.end_restore();
    }
}
