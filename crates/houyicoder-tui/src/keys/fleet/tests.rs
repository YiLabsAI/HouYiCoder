use super::*;

/// next_selection snaps None to 0 and clamps at the bounds without
/// wrapping, including the empty-fleet guard.
#[test]
fn test_selection_clamps_at_bounds() {
    assert_eq!(next_selection(None, 3, 1), 0);
    assert_eq!(next_selection(Some(2), 3, 1), 2);
    assert_eq!(next_selection(Some(0), 3, -1), 0);
    assert_eq!(next_selection(None, 3, -1), 0);
    assert_eq!(next_selection(None, 0, 1), 0);
}

mod focus {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::agent_message::FleetEntry;
    use crate::state::App;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn child(id: &str) -> FleetEntry {
        FleetEntry {
            agent_id: id.into(),
            subagent_type: "verify".into(),
            turn: 1,
            tokens: 0,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
        }
    }

    fn app_with(children: &[&str]) -> App {
        let mut app = crate::composition::app();
        for id in children {
            app.sessions
                .current
                .capabilities
                .fleet
                .entries
                .push(child(id));
        }
        app
    }

    /// Moving down out of the input box puts the keyboard on the strip. A
    /// strip with no rows never takes it.
    #[test]
    fn test_focus_handoff() {
        let mut app = app_with(&[]);
        assert!(
            !super::focus_strip(&mut app.ui.fleet, 0),
            "an empty strip takes nothing"
        );
        assert_eq!(app.ui.fleet.selected(), None);

        let mut app = app_with(&["c1", "c2"]);
        assert!(super::focus_strip(&mut app.ui.fleet, 2));
        assert_eq!(app.ui.fleet.selected(), Some(0));
        // The selection survives leaving and coming back.
        app.ui.fleet.set_selected(Some(1));
        assert!(super::focus_strip(&mut app.ui.fleet, 2));
        assert_eq!(app.ui.fleet.selected(), Some(1));
    }

    /// Enter on the strip names the child to open, so the caller opens it
    /// without the draft ever being sent.
    #[test]
    fn test_strip_enter_keeps_draft() {
        let mut app = app_with(&["c1"]);
        assert!(super::focus_strip(&mut app.ui.fleet, 1));
        let entries = &app.sessions.current.capabilities.fleet.entries;
        let decision = super::handle_focused(&mut app.ui.fleet, entries, key(KeyCode::Enter));
        assert!(
            matches!(decision, super::FleetKey::Open(ref sid) if sid == "c1"),
            "the strip names the child to open"
        );
    }

    /// Esc leaves the strip without touching the draft.
    #[test]
    fn test_esc_returns_to_input() {
        let mut app = app_with(&["c1"]);
        let entries = &app.sessions.current.capabilities.fleet.entries;
        assert!(matches!(
            super::handle_focused(&mut app.ui.fleet, entries, key(KeyCode::Esc)),
            super::FleetKey::Leave
        ));
    }

    /// A printable key is handed back to the draft, so typing a correction
    /// reads as typing rather than as a swallowed keystroke.
    #[test]
    fn test_typing_leaves_strip() {
        let mut app = app_with(&["c1"]);
        let entries = &app.sessions.current.capabilities.fleet.entries;
        assert!(
            matches!(
                super::handle_focused(&mut app.ui.fleet, entries, key(KeyCode::Char('a'))),
                super::FleetKey::PassThrough
            ),
            "the key is not consumed by the strip"
        );
    }

    /// Down at the last row stops there instead of wrapping onto the first,
    /// and Up at the top does not go above it.
    #[test]
    fn test_down_clamps() {
        let mut app = app_with(&["c1", "c2"]);
        app.ui.fleet.set_selected(Some(1));
        let entries = &app.sessions.current.capabilities.fleet.entries;
        assert!(matches!(
            super::handle_focused(&mut app.ui.fleet, entries, key(KeyCode::Down)),
            super::FleetKey::Stay
        ));
        assert_eq!(app.ui.fleet.selected(), Some(1));

        app.ui.fleet.set_selected(Some(0));
        assert!(matches!(
            super::handle_focused(&mut app.ui.fleet, entries, key(KeyCode::Up)),
            super::FleetKey::Leave
        ));
        assert_eq!(app.ui.fleet.selected(), Some(0));
    }
}

/// The owner applies what the strip decided: Enter opens the child, Esc keeps
/// the draft and gives the keyboard back, and any other key falls through so
/// the draft receives it.
mod dispatch {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::agent_message::FleetEntry;
    use crate::state::App;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_with_child() -> App {
        let mut app = crate::composition::app();
        app.sessions
            .current
            .capabilities
            .fleet
            .entries
            .push(FleetEntry {
                agent_id: "c1".into(),
                subagent_type: "verify".into(),
                turn: 1,
                tokens: 0,
                tool_uses: 0,
                last_activity: None,
                completed: None,
                completed_at: None,
                started_at: None,
            });
        app.ui.fleet.focus(0);
        app
    }

    #[test]
    fn test_enter_opens_child() {
        let mut app = app_with_child();
        app.ui.input.editor.set("keep me".into());
        assert!(app.dispatch_fleet_key(key(KeyCode::Enter)));
        assert!(app.ui.teammate_view.is_some(), "the child opens");
        assert!(
            !app.ui.fleet.is_focused(),
            "the keyboard is not left on a strip the user has left"
        );
        assert_eq!(app.ui.input.editor.value(), "keep me");
    }

    #[test]
    fn test_esc_keeps_draft() {
        let mut app = app_with_child();
        app.ui.input.editor.set("draft".into());
        assert!(app.dispatch_fleet_key(key(KeyCode::Esc)));
        assert!(!app.ui.fleet.is_focused());
        assert!(app.ui.teammate_view.is_none(), "Esc does not open anything");
        assert_eq!(app.ui.input.editor.value(), "draft");
    }

    /// Two presses inside the strip both move. A dispatcher that gives the
    /// keyboard back after the first press turns the second Down into draft
    /// history recall, which is why the decision distinguishes staying from
    /// leaving.
    #[test]
    fn test_arrows_stay_focused() {
        let mut app = app_with_child();
        app.sessions
            .current
            .capabilities
            .fleet
            .entries
            .push(FleetEntry {
                agent_id: "c2".into(),
                subagent_type: "explore".into(),
                turn: 1,
                tokens: 0,
                tool_uses: 0,
                last_activity: None,
                completed: None,
                completed_at: None,
                started_at: None,
            });
        app.ui.fleet.focus(0);
        app.ui.input.editor.set("draft".into());

        assert!(app.dispatch_fleet_key(key(KeyCode::Down)));
        assert_eq!(app.ui.fleet.selected(), Some(1), "the first press moves");
        assert!(app.ui.fleet.is_focused(), "and keeps the keyboard");

        assert!(app.dispatch_fleet_key(key(KeyCode::Up)));
        assert_eq!(app.ui.fleet.selected(), Some(0), "the second press moves");
        assert!(app.ui.fleet.is_focused(), "and keeps the keyboard");
        assert_eq!(
            app.ui.input.editor.value(),
            "draft",
            "the draft is untouched while the strip has the keys"
        );
    }

    #[test]
    fn test_char_falls_through() {
        let mut app = app_with_child();
        assert!(!app.dispatch_fleet_key(key(KeyCode::Char('x'))));
        assert!(!app.ui.fleet.is_focused());
        assert_eq!(
            app.ui.fleet.selected(),
            Some(0),
            "the highlight stays for the way back"
        );
    }
}
