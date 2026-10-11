//! The /model pane commands: opening the picker, moving the model focus,
//! adjusting the settings, committing the draft, and applying the host's
//! reply. Nothing here mutates applied state on its own - the commit ships a
//! request and the reply is what moves the session. The catalog facts live on
//! the session's model capability and the draft lives on the Ui owner, so
//! every operation spanning both is one method here: the read of the facts
//! and the write of the draft cannot drift apart at separate call sites.

use crate::run_control::ClientCommand;
use crate::session::ConnectionStatus;
use crate::session::capabilities::model::PendingCommit;
use crate::state::model_draft::ModelSettingFocus;
use crate::state::{App, Pane};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::model::{ModelApplyResult, ModelCatalog, SpeedMode};

impl App {
    /// Open the picker on the session's current pick and ask the host for a
    /// fresh catalog. The draft starts as a copy of what is applied, so the
    /// pane shows the session's state rather than the last thing touched.
    pub(crate) fn open_model_pane(&mut self) {
        self.ui.navigation.pane = Pane::Model;
        self.reseed_model_draft();
        if let Some(s) = self.sessions.current.connection.as_ref() {
            match s.next_request_id() {
                Ok(req_id) => {
                    self.enqueue_refresh(ClientCommand::ModelInfoQuery { req_id });
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
    }

    /// Reset the draft from the session's catalog: the row the session is set
    /// to, the level that row's chain resolves, the applied tier, focus on
    /// effort.
    pub(crate) fn reseed_model_draft(&mut self) {
        let cap = &self.sessions.current.capabilities.model;
        let row = cap.row_for_choice(&cap.catalog.selected);
        let speed = cap.catalog.applied.speed;
        let effort = cap.seed_effort(row);
        self.ui.model.reseed(row, speed, effort);
    }

    /// Take a fresh catalog from the host, keeping a dirty draft: the user's
    /// in-progress pick wins over a query reply that raced it.
    pub(crate) fn refresh_model_catalog(&mut self, catalog: ModelCatalog) {
        self.sessions.current.capabilities.model.catalog = catalog;
        if !self.ui.model.dirty {
            self.reseed_model_draft();
        }
    }

    /// The tier the draft would actually send: a tier the focused model
    /// does not offer commits as Standard, the same value the pane reports.
    pub(crate) fn effective_model_speed(&self) -> SpeedMode {
        let cap = &self.sessions.current.capabilities.model;
        let fast_available = cap.fast_available_at(self.ui.model.row);
        self.ui.model.effective_speed(fast_available)
    }

    /// Move the model focus by delta rows. The level follows the newly
    /// focused model's chain: an explicit pick made on one row does not carry
    /// to another.
    pub(crate) fn move_model_focus(&mut self, delta: isize) {
        let cap = &self.sessions.current.capabilities.model;
        if !self.ui.model.move_focus(delta, cap.rows()) {
            return;
        }
        let effort = cap.seed_effort(self.ui.model.row);
        self.ui.model.effort = effort;
    }

    /// Move the setting focus to the other adjustable setting, skipping one
    /// the focused model does not support.
    pub(crate) fn cycle_model_setting(&mut self) {
        let cap = &self.sessions.current.capabilities.model;
        let row = self.ui.model.row;
        let effort_available = cap.effort_supported_at(row);
        let fast_available = cap.fast_available_at(row);
        self.ui.model.cycle_focus(effort_available, fast_available);
    }

    /// Adjust the focused setting by one step: the level cycles through the
    /// levels the focused row's model offers and the tier toggles off/on.
    /// Both are no-ops when the focused model does not support the setting.
    pub(crate) fn adjust_model_setting(&mut self, forward: bool) {
        let cap = &self.sessions.current.capabilities.model;
        let row = self.ui.model.row;
        match self.ui.model.focus {
            ModelSettingFocus::Effort => {
                let levels = cap.effort_levels_at(row);
                self.ui.model.adjust_effort(&levels, forward);
            }
            ModelSettingFocus::Fast => {
                let fast_available = cap.fast_available_at(row);
                self.ui.model.toggle_speed(fast_available);
            }
        }
    }

    /// Discard the draft and close the pane. A commit already with the host
    /// is not discarded: it is about to change the session, so the pane stays
    /// until the reply says what it did.
    pub(crate) fn discard_model_pick(&mut self) {
        if self.sessions.current.capabilities.model.is_pending() {
            return;
        }
        self.reseed_model_draft();
        self.ui.navigation.pane = Pane::Transcript;
    }

    /// Commit the draft: one request carries the model, the effort and the
    /// speed tier, and the pane holds it until the host answers.
    pub(crate) fn commit_model_pick(&mut self) {
        if self.sessions.current.capabilities.model.is_pending() {
            return;
        }
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("model: not connected");
            return;
        };
        // A lost connection reports the failed action rather than silently
        // no-oping on the empty-catalog guard below.
        if matches!(s.status(), ConnectionStatus::Lost(_)) {
            self.system_line("model: connection lost");
            return;
        }
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("model: request ids exhausted");
            return;
        };
        // An empty catalog means the query reply has not landed; committing
        // off it would send the Default sentinel and the pre-reply speed, a
        // switch the user never saw.
        let cap = &self.sessions.current.capabilities.model;
        if cap.catalog.entries.is_empty() {
            return;
        }
        let choice = cap.choice_at(self.ui.model.row);
        let command = ClientCommand::ModelSwitch {
            req_id,
            model: choice.explicit_id().map(str::to_string),
            effort: self.ui.model.effort,
            effort_toggled: self.ui.model.effort_touched,
            speed: Some(self.effective_model_speed()),
        };
        let prior_speed = cap.catalog.applied.speed;
        // Send before recording the commit: a dead driver must not leave a
        // pending switch that never shipped.
        if let Err(e) = s.enqueue(command) {
            self.system_line(Self::enqueue_failure_line("model", e));
            return;
        }
        self.sessions.current.capabilities.model.pending = Some(PendingCommit {
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
        let cap = &self.sessions.current.capabilities.model;
        let expected = cap.pending.as_ref().map(|p| p.req_id);
        let settles = expected == Some(req_id);
        let label = cap.label_for(&result.selected);
        let prior_speed = if settles {
            cap.pending.as_ref().expect("checked above").prior_speed
        } else {
            result.applied.speed
        };
        let fast_available = cap.capabilities_for(&result.applied.id).fast.is_available();
        let receipt =
            crate::model_receipt::receipt_line(&result, &label, prior_speed, fast_available);
        if settles {
            self.sessions.current.capabilities.model.pending = None;
            self.ui.navigation.pane = Pane::Transcript;
        }
        self.sessions.current.capabilities.model.catalog.applied = result.applied.clone();
        self.sessions.current.capabilities.model.catalog.selected = result.selected.clone();
        // Only the settled commit reseeds; a stale reply must not wipe a
        // draft the user is mid-edit on.
        if settles || !self.ui.model.dirty {
            self.reseed_model_draft();
        }
        self.status.model = result.applied.id;
        self.system_line(receipt);
    }

    /// A commit the host rejected. The draft survives for a retry and the
    /// reason goes to the transcript; the pane stays open.
    pub(crate) fn fail_model_pick(&mut self, req_id: RequestId, message: &str) {
        let expected = self
            .sessions
            .current
            .capabilities
            .model
            .pending
            .as_ref()
            .map(|p| p.req_id);
        if expected != Some(req_id) {
            return;
        }
        self.sessions.current.capabilities.model.pending = None;
        self.system_line(format!("model: {message}"));
    }
}
