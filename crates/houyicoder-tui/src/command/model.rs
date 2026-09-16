//! Applies the selected model and effort through the active session.

use houyicoder_protocol::llm::EffortLevel;

use crate::run_control::ClientCommand;
use crate::state::{App, Pane};
use crate::view::model_pane::{model_id_at, supports_effort};

impl App {
    /// On cursor move (Up/Down), recompute the effort pick to follow the new
    /// focused model's default — but only if the user has NOT toggled effort
    /// this session. Once toggled, the pick sticks across rows.
    pub(crate) fn recompute_effort_on_cursor_move(&mut self) {
        if self.model_effort_toggled {
            return;
        }
        let model = model_id_at(self, self.model_sel);
        let id = model
            .as_deref()
            .or(self.model_catalog.active_id.as_deref())
            .unwrap_or("");
        self.model_effort = if supports_effort(id) {
            Some(EffortLevel::Medium)
        } else {
            None
        };
    }

    /// Cycle the effort pick left (false) or right (true), wrapping around.
    /// Sets model_effort_toggled = true so subsequent cursor moves don't
    /// clobber the pick. No-op when the focused model is NotSupported.
    pub(crate) fn cycle_effort(&mut self, forward: bool) {
        let model = model_id_at(self, self.model_sel);
        let id = model
            .as_deref()
            .or(self.model_catalog.active_id.as_deref())
            .unwrap_or("");
        if !supports_effort(id) {
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
        // No request, no switch: the tier stays server-authoritative, the
        // pane stays open for retry, and no success line is pushed.
        let Some(s) = self.session.as_ref() else {
            self.system_line("model: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("model: request ids exhausted");
            return;
        };
        let idx = self.model_sel;
        let id = model_id_at(self, idx);
        let tier = id
            .clone()
            .or_else(|| Some("Default".into()))
            .unwrap_or_else(|| "Default".into());
        let command = ClientCommand::ModelSwitch {
            req_id,
            model: id.clone(),
            effort: self.model_effort,
            effort_toggled: self.model_effort_toggled,
        };
        // Send before touching any local state: a dead driver must not leave
        // the tier moved with no request sent.
        if let Err(e) = self.enqueue(command) {
            self.system_line(Self::enqueue_failure_line("model", e));
            return;
        }
        self.model_tier = tier.clone();
        // status.model holds the resolved concrete (for the status pane +
        // snapshot, which show what is running). Set it for a concrete pick;
        // for Default the ModelResult reply fills the resolved value. The
        // status BAR reads model_tier via status_bar_model(), not this.
        if let Some(concrete) = id {
            self.status.model = concrete;
        }
        self.pane = Pane::Transcript;
        self.system_line(format!("model: {tier}"));
    }
}
