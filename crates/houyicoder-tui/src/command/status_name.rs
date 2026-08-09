//! In-place session-name edit on the /status Status tab. Split out of
//! command.rs on size grounds (same pattern as resume / model). The user
//! presses the e key on the Status tab to enter edit, types into an
//! InputField, Enter commits (ships a RenameSession request), Esc cancels.
//! Houyi makes the session name inline-editable + syncs the terminal tab
//! title via OSC 0/2 on the reply (rather than a rename command).

use crate::input::InputField;
use crate::state::App;

impl App {
    /// Enter the name-edit mode on the /status Status tab. The buffer starts
    /// empty: the displayed name may be an Auto-derived slug (the server
    /// derives it for display when name_source=Auto), and pre-filling it
    /// would pin the slug as name_source=User the moment the user pressed
    /// Enter without typing. An empty buffer + Enter clears to Auto (no
    /// pin); typing a name + Enter sets User. Opens unconditionally; the
    /// session check lives at commit (the editor is harmless without a
    /// session -- Enter reports stub mode then).
    pub(crate) fn enter_status_name_edit(&mut self) {
        self.status_name_edit = Some(InputField::new());
    }

    /// Cancel the name edit: drop the buffer, no request shipped.
    pub(crate) fn cancel_status_name_edit(&mut self) {
        self.status_name_edit = None;
    }

    /// Commit the name edit: ship a RenameSession request with the buffer
    /// contents, then drop the editor. The reply lands as a StatusResult,
    /// which refreshes the pane + the terminal tab title. An empty buffer
    /// clears the name back to Auto (the server derives the slug).
    pub(crate) fn commit_status_name_edit(&mut self) {
        let Some(field) = self.status_name_edit.take() else {
            return;
        };
        let name = field.value().to_string();
        let Some(s) = self.session.as_ref() else {
            self.system_line("rename: no session wired (stub mode)");
            return;
        };
        s.request_rename(self.session_id.clone(), name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// enter opens the editor even without a session wired; the session check
    /// lives at commit (a stub-mode app can still open the editor; Enter reports
    /// stub mode there).
    #[test]
    fn test_enter_opens_without_session() {
        let mut app = crate::composition::app();
        app.session = None;
        app.enter_status_name_edit();
        assert!(
            app.status_name_edit.is_some(),
            "editor opens without a session (commit is the gate)"
        );
    }

    /// cancel drops the editor without shipping a request.
    #[test]
    fn test_cancel_drops_editor() {
        let mut app = crate::composition::app();
        app.status_name_edit = Some(InputField::new());
        app.cancel_status_name_edit();
        assert!(app.status_name_edit.is_none());
    }

    /// commit with no editor is a no-op (does not panic).
    #[test]
    fn test_commit_no_editor_noop() {
        let mut app = crate::composition::app();
        app.commit_status_name_edit();
        assert!(app.status_name_edit.is_none());
    }
}
