//! /model select: apply the row at the cursor (Default sentinel or a catalog
//! id) to the active model. Sends a ModelSwitch over the wire; the host
//! resolves + persists + replies with the applied ModelApplied.

use crate::state::App;
use houyicoder_protocol::llm::EffortLevel;

impl App {
    /// On cursor move (Up/Down), recompute the effort pick to follow the new
    /// focused model's default — but only if the user has NOT toggled effort
    /// this session. Once toggled, the pick sticks across rows.
    pub(crate) fn recompute_effort_on_cursor_move(&mut self) {
        if self.model_effort_toggled {
            return;
        }
        let model = crate::view::model_pane::model_id_at(self, self.model_sel);
        let id = model
            .as_deref()
            .or(self.model_catalog.active_id.as_deref())
            .unwrap_or("");
        self.model_effort = if crate::view::model_pane::supports_effort(id) {
            Some(EffortLevel::Medium)
        } else {
            None
        };
    }

    /// Cycle the effort pick left (false) or right (true), wrapping around.
    /// Sets model_effort_toggled = true so subsequent cursor moves don't
    /// clobber the pick. No-op when the focused model is NotSupported.
    pub(crate) fn cycle_effort(&mut self, forward: bool) {
        let model = crate::view::model_pane::model_id_at(self, self.model_sel);
        let id = model
            .as_deref()
            .or(self.model_catalog.active_id.as_deref())
            .unwrap_or("");
        if !crate::view::model_pane::supports_effort(id) {
            return;
        }
        let levels = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
        let current = self.model_effort.unwrap_or(EffortLevel::Medium);
        let idx = levels.iter().position(|l| *l == current).unwrap_or(1);
        let next = if forward {
            (idx + 1) % levels.len()
        } else {
            (idx + levels.len() - 1) % levels.len()
        };
        self.model_effort = Some(levels[next]);
        self.model_effort_toggled = true;
    }

    pub(crate) fn set_model_at_cursor(&mut self) {
        let idx = self.model_sel;
        let id = crate::view::model_pane::model_id_at(self, idx);
        // For the Default sentinel (id=None), do not set status.model here —
        // the server resolves Default to DEFAULT_MODEL and the ModelResult
        // reply carries the resolved id. Setting it to "Default" here causes
        // a visible flicker (Default → resolved model). For a concrete id,
        // set it immediately (the reply echoes the same id, no flicker).
        if let Some(ref concrete) = id {
            self.status.model = concrete.clone();
        }
        let tier = id
            .clone()
            .or_else(|| Some("Default".into()))
            .unwrap_or_else(|| "Default".into());
        self.model_tier = tier.clone();
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(crate::run_control::ClientCommand::ModelSwitch {
                req_id,
                model: id,
                effort: self.model_effort,
                effort_toggled: self.model_effort_toggled,
            });
        }
        self.pane = crate::state::Pane::Transcript;
        self.system_line(format!("model: {tier}"));
    }
}
