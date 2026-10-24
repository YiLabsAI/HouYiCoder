//! In-place session-name edit on the /status Status tab. Split out of
//! command.rs on size grounds (same pattern as resume / model). The user
//! presses the e key on the Status tab to enter edit, types into an
//! InputField, Enter sends a RenameSession request, and Esc cancels.
//! Houyi makes the session name inline-editable + syncs the terminal tab
//! title via OSC 0/2 on the reply (rather than a rename command).

use crate::agent_message::ClientCommand;
use crate::input::InputField;
use crate::state::App;

impl App {
    /// Enter the name-edit mode on the /status Status tab. The buffer starts
    /// empty: the displayed name may be an Auto-derived slug (the server
    /// derives it for display when name_source=Auto), and pre-filling it
    /// would pin the slug as name_source=User the moment the user pressed
    /// Enter without typing. An empty buffer + Enter clears to Auto (no
    /// pin); typing a name + Enter sets User. Opens unconditionally; the
    /// session check lives at commit (the editor is harmless while
    /// disconnected -- Enter reports not connected then).
    pub(crate) fn enter_status_name_edit(&mut self) {
        self.ui.navigation.status_pane.name_edit = Some(InputField::new());
    }

    /// Cancel the name edit without sending a request.
    pub(crate) fn cancel_status_name_edit(&mut self) {
        self.ui.navigation.status_pane.name_edit = None;
    }

    /// Send the buffered name; an empty value restores automatic naming.
    pub(crate) fn commit_status_name_edit(&mut self) {
        let Some(field) = self.ui.navigation.status_pane.name_edit.take() else {
            return;
        };
        let name = field.value().to_string();
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("rename: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("rename: request ids exhausted");
            return;
        };
        if let Err(e) = s.enqueue(ClientCommand::RenameSessionQuery {
            req_id,
            session_id: self.sessions.current.identity.session_id.clone(),
            name,
        }) {
            self.system_line(Self::enqueue_failure_line("rename", e));
        }
    }
}

#[cfg(test)]
mod tests;
