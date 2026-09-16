//! The /model pane commands: opening the picker, moving the model focus,
//! adjusting the settings, committing the draft, and applying the host's
//! reply. Nothing here mutates applied state on its own - the commit ships a
//! request and the reply is what moves the session.

use crate::run_control::ClientCommand;
use crate::session::ConnectionStatus;
use crate::state::{App, Pane, PendingCommit};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::model::ModelApplyResult;

impl App {
    /// Open the picker on the session's current pick and ask the host for a
    /// fresh snapshot. The draft starts as a copy of what is applied, so the
    /// pane shows the session's state rather than the last thing touched.
    pub(crate) fn open_model_pane(&mut self) {
        self.pane = Pane::Model;
        self.model_picker.reseed();
        if let Some(s) = self.session.as_ref() {
            match s.next_request_id() {
                Ok(req_id) => {
                    self.enqueue_refresh(ClientCommand::ModelInfoQuery { req_id });
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
    }

    /// Move the model focus by delta rows.
    pub(crate) fn move_model_focus(&mut self, delta: isize) {
        self.model_picker.move_focus(delta);
    }

    /// Move the setting focus to the other adjustable setting.
    pub(crate) fn cycle_model_setting(&mut self) {
        self.model_picker.cycle_setting_focus();
    }

    /// Adjust the focused setting by one step.
    pub(crate) fn adjust_model_setting(&mut self, forward: bool) {
        self.model_picker.adjust_setting(forward);
    }

    /// Discard the draft and close the pane. A commit already with the host
    /// is not discarded: it is about to change the session, so the pane stays
    /// until the reply says what it did.
    pub(crate) fn discard_model_pick(&mut self) {
        if self.model_picker.is_pending() {
            return;
        }
        self.model_picker.reseed();
        self.pane = Pane::Transcript;
    }

    /// Commit the draft: one request carries the model, the effort and the
    /// speed tier, and the pane holds it until the host answers.
    pub(crate) fn commit_model_pick(&mut self) {
        if self.model_picker.is_pending() {
            return;
        }
        let Some(s) = self.session.as_ref() else {
            self.system_line("model: not connected");
            return;
        };
        // A lost connection reports the failed action rather than silently
        // no-oping on the empty-snapshot guard below.
        if matches!(s.status(), ConnectionStatus::Lost(_)) {
            self.system_line("model: connection lost");
            return;
        }
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("model: request ids exhausted");
            return;
        };
        // An empty snapshot means the query reply has not landed; committing
        // off it would send the Default sentinel + the pre-snapshot speed, a
        // switch the user never saw.
        if self.model_picker.snapshot.entries.is_empty() {
            return;
        }
        let choice = self.model_picker.focused_choice();
        let command = ClientCommand::ModelSwitch {
            req_id,
            model: choice.explicit_id().map(str::to_string),
            effort: self.model_picker.draft.effort,
            effort_toggled: self.model_picker.draft.effort_touched,
            speed: Some(self.model_picker.effective_speed()),
        };
        let prior_speed = self.model_picker.snapshot.applied.speed;
        // Send before recording the commit: a dead driver must not leave a
        // pending switch that never shipped.
        if let Err(e) = s.enqueue(command) {
            self.system_line(Self::enqueue_failure_line("model", e));
            return;
        }
        self.model_picker.pending_request = Some(PendingCommit {
            req_id,
            prior_speed,
        });
    }

    /// Apply the host's answer. A reply for the commit the pane is holding
    /// settles it: the receipt is formatted from the reply and the pane
    /// closes. A reply the pane is not holding (a stale reply after a
    /// reconnect) still moves the session state but leaves the pending
    /// commit, the pane and a draft the user is editing alone — the held
    /// commit's own reply decides those.
    pub(crate) fn apply_model_result(&mut self, req_id: RequestId, result: ModelApplyResult) {
        let expected = self.model_picker.pending_request.as_ref().map(|p| p.req_id);
        let settles = expected == Some(req_id);
        let label = self.model_picker.label_for(&result.selected);
        let prior_speed = if settles {
            self.model_picker
                .pending_request
                .as_ref()
                .expect("checked above")
                .prior_speed
        } else {
            result.applied.speed
        };
        let fast_available = self
            .model_picker
            .capabilities_for(&result.applied.id)
            .fast
            .is_available();
        let receipt =
            crate::model_receipt::receipt_line(&result, &label, prior_speed, fast_available);
        if settles {
            self.model_picker.settle(None);
            self.pane = Pane::Transcript;
        }
        self.model_picker.snapshot.applied = result.applied.clone();
        self.model_picker.snapshot.selected = result.selected.clone();
        // Only the settled commit reseeds; a stale reply must not wipe a
        // draft the user is mid-edit on.
        if settles || !self.model_picker.draft.dirty {
            self.model_picker.reseed();
        }
        self.status.model = result.applied.id;
        self.system_line(receipt);
    }

    /// A commit the host rejected. The draft survives for a retry and the
    /// reason goes to the transcript; the pane stays open.
    pub(crate) fn fail_model_pick(&mut self, req_id: RequestId, message: &str) {
        let Some(pending) = &self.model_picker.pending_request else {
            return;
        };
        if pending.req_id != req_id {
            return;
        }
        self.model_picker.settle(None);
        self.system_line(format!("model: {message}"));
    }
}
