//! In-place session-name edit on the /status Status tab. Split out of
//! command.rs on size grounds (same pattern as resume / model). The user
//! presses the e key on the Status tab to enter edit, types into an
//! InputField, Enter sends a RenameSession request, and Esc cancels.
//! Houyi makes the session name inline-editable + syncs the terminal tab
//! title via OSC 0/2 on the reply (rather than a rename command).

use crate::input::InputField;
use crate::run_control::ClientCommand;
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
        self.status_name_edit = Some(InputField::new());
    }

    /// Cancel the name edit without sending a request.
    pub(crate) fn cancel_status_name_edit(&mut self) {
        self.status_name_edit = None;
    }

    /// Send the buffered name; an empty value restores automatic naming.
    pub(crate) fn commit_status_name_edit(&mut self) {
        let Some(field) = self.status_name_edit.take() else {
            return;
        };
        let name = field.value().to_string();
        let Some(s) = self.session.as_ref() else {
            self.system_line("rename: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("rename: request ids exhausted");
            return;
        };
        if !s.send(ClientCommand::RenameSessionQuery {
            req_id,
            session_id: self.session_id.clone(),
            name,
        }) {
            self.system_line("rename: connection lost");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_harness::{connected_app_events, wait_for_request};

    /// enter opens the editor even without an active session; the session
    /// check lives at commit (a disconnected app can still open the editor;
    /// Enter reports not connected there).
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

    /// Commit with a typed name ships a RenameSession request under the
    /// current session id.
    #[test]
    fn test_commit_ships_rename() {
        use houyicoder_protocol::frontend::FrontendRequest;
        let (mut app, events) = connected_app_events();
        let mut field = InputField::new();
        field.set("my session".into());
        app.status_name_edit = Some(field);
        app.commit_status_name_edit();
        let req = wait_for_request(&events, |p| {
            matches!(p, FrontendRequest::RenameSession { .. })
        });
        assert_eq!(req.req_id.0, 0, "first request on a fresh session");
        match req.payload {
            FrontendRequest::RenameSession { name, .. } => assert_eq!(name, "my session"),
            other => panic!("unexpected request: {other:?}"),
        }
    }
}
