//! Cursor-priority preselect for the tool-approval popup. The initial cursor
//! is chosen by priority: a sticky last-used verdict for this tool (matched by
//! identity, not list position); then YOLO when the permission mode
//! auto-approves (Auto) focuses the quickest approve; otherwise index-0. A
//! per-tool configured default would sit between sticky and YOLO, but no
//! config schema exists for it yet.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::composition;
use crate::state::{Approval, Screen};

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn approval_at(tool: &str, selected: usize) -> Approval {
    Approval {
        tool: tool.to_string(),
        args: String::new(),
        reason: String::new(),
        selected,
        call_id: "c1".to_string(),
        options: Vec::new(),
        ..Default::default()
    }
}

#[test]
fn test_cursor_defaults_zero() {
    // No sticky choice, no auto-approve mode: the cursor lands on Yes (0),
    // the safe default.
    let app = composition::app();
    assert_eq!(app.initial_cursor("bash"), 0);
}

#[test]
fn test_reject_records_sticky() {
    // Rejecting bash via Esc records a sticky reject-once. The next popup for
    // bash preselects No (index 1), so a user who keeps rejecting bash is not
    // bounced back to Yes each time.
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.approval = Some(approval_at("bash", 0));
    crate::keys::handle_working(&mut app, key(KeyCode::Esc));
    use houyicoder_protocol::acp_wire::PermissionOptionKind;
    assert_eq!(
        app.sticky_choices.get("bash"),
        Some(&PermissionOptionKind::RejectOnce),
        "Esc records a sticky reject",
    );
    assert_eq!(app.initial_cursor("bash"), 1, "sticky reject preselects No");
    assert_eq!(app.initial_cursor("read"), 0, "unrelated tool unaffected");
}

#[test]
fn test_persist_records_sticky() {
    // Approving bash with Yes-don't-ask (index 2) records a sticky
    // allow-always; the next popup preselects index 2.
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.approval = Some(approval_at("bash", 2));
    crate::keys::handle_working(&mut app, key(KeyCode::Enter));
    use houyicoder_protocol::acp_wire::PermissionOptionKind;
    assert_eq!(
        app.sticky_choices.get("bash"),
        Some(&PermissionOptionKind::AllowAlways),
        "Enter on Yes-dont-ask records allow-always",
    );
    assert_eq!(
        app.initial_cursor("bash"),
        2,
        "sticky allow-always preselects index 2",
    );
}

#[test]
fn test_auto_focuses_approve() {
    // Auto-approve mode (Auto) focuses Yes (0) when no sticky choice exists
    // for the tool, matching the user's minimal-friction intent.
    use houyicoder_protocol::frontend::permission::PermissionMode;
    let mut app = composition::app();
    app.mode_cache = Some(PermissionMode::Auto);
    assert_eq!(app.initial_cursor("bash"), 0);
}

#[test]
fn test_sticky_beats_auto() {
    // A sticky choice takes priority over the auto-approve mode: a user who
    // rejected bash still lands on No even in Auto mode.
    use houyicoder_protocol::acp_wire::PermissionOptionKind;
    use houyicoder_protocol::frontend::permission::PermissionMode;
    let mut app = composition::app();
    app.mode_cache = Some(PermissionMode::Auto);
    app.sticky_choices
        .insert("bash".to_string(), PermissionOptionKind::RejectOnce);
    assert_eq!(
        app.initial_cursor("bash"),
        1,
        "sticky reject beats Auto-mode preselect",
    );
}
