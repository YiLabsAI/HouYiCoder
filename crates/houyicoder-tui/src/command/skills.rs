//! Skill pane actions: opening a detail and flipping the session disable.
//!
//! Both resolve through the pane's own cursor, so the row the user sees
//! selected is the row acted on. Opening settles the pending state it
//! creates when the request cannot be sent.

use crate::agent_message::ClientCommand;
use crate::state::App;
use crate::view::skills_pane::display_order;

impl App {
    /// Open the detail for the skill the list cursor points at, asking for
    /// the body by name. A session that cannot allocate a request id, or a
    /// send that fails, settles the detail with the body unavailable instead
    /// of leaving it loading on a reply that will never come.
    pub(crate) fn open_skill_detail(&mut self) {
        let ordered = display_order(&self.skill_entries);
        let selected = self
            .skills_pane
            .cursor()
            .min(ordered.len().saturating_sub(1));
        let Some(entry) = ordered.get(selected) else {
            return;
        };
        let name = entry.name.clone();
        let Some(session) = self.session.as_ref() else {
            self.skills_pane.open_unavailable(name);
            return;
        };
        let Ok(req_id) = session.next_request_id() else {
            self.skills_pane.open_unavailable(name);
            return;
        };
        self.skills_pane.request_detail(req_id, name.clone());
        let query = ClientCommand::SkillBodyQuery { req_id, name };
        if self.enqueue(query).is_err() {
            self.skills_pane.fail_detail(req_id);
        }
    }

    /// Flip the session disable for the skill the list cursor points at.
    /// Only a skill the user or the model can invoke has a state to flip; a
    /// frontmatter-blocked skill stays blocked until its file changes.
    pub(crate) fn toggle_skill_at_cursor(&mut self) {
        let ordered = display_order(&self.skill_entries);
        let selected = self
            .skills_pane
            .cursor()
            .min(ordered.len().saturating_sub(1));
        let Some(entry) = ordered.get(selected) else {
            return;
        };
        if !(entry.user_invocable || entry.invocable) {
            return;
        }
        if !self.skill_disabled.insert(entry.name.clone()) {
            self.skill_disabled.remove(&entry.name);
        }
    }
}
