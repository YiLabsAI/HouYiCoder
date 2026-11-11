use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use serde_json::json;

use super::KeyFilter;
use crate::agent_message::fleet::FleetEntry;
use crate::ask_question_model::AskQuestion;
use crate::pending_prompt::PendingPrompt;
use crate::state::{App, Pane, PermissionInput, Screen, ViewportMode};
use crate::test_harness::working_app;

fn app() -> App {
    let mut app = working_app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Transcript;
    app.ui.navigation.viewport = ViewportMode::Working;
    app
}

fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
    KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
}

fn control(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
    KeyEvent::new_with_kind(code, KeyModifiers::CONTROL, kind)
}

/// A release is the tail of a press and must never act on its own.
#[test]
fn test_release_never_acts() {
    let mut filter = KeyFilter::default();
    let app = app();
    let now = Instant::now();
    assert!(filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Press), now));
    assert!(!filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Release), now));
    assert!(!filter.admit(&app, &key(KeyCode::Down, KeyEventKind::Release), now));
    assert!(!filter.admit(
        &app,
        &control(KeyCode::Char('o'), KeyEventKind::Release),
        now
    ));
}

/// A press acts once and its release adds nothing.
#[test]
fn test_press_acts_once() {
    let mut filter = KeyFilter::default();
    let app = app();
    let now = Instant::now();
    assert!(filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Press), now));
    assert!(!filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Release), now));
    assert!(filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Press), now));
}

/// A held character repeats where it types, so typing stays responsive.
#[test]
fn test_text_repeat_kept() {
    let mut filter = KeyFilter::default();
    let app = app();
    let now = Instant::now();
    for code in [
        KeyCode::Char('h'),
        KeyCode::Down,
        KeyCode::Backspace,
        KeyCode::PageUp,
    ] {
        assert!(
            filter.admit(&app, &key(code, KeyEventKind::Repeat), now),
            "{code:?} repeats while typing"
        );
    }
}

/// A held chord is a repeated action, not repeated text: the one-shot keys
/// fire on the press alone.
#[test]
fn test_chord_repeat_ignored() {
    let mut filter = KeyFilter::default();
    let app = app();
    let now = Instant::now();
    for code in [KeyCode::Char('o'), KeyCode::Char('d'), KeyCode::Char('c')] {
        assert!(
            !filter.admit(&app, &control(code, KeyEventKind::Repeat), now),
            "ctrl+{code:?} must not repeat"
        );
    }
    assert!(!filter.admit(&app, &key(KeyCode::Esc, KeyEventKind::Repeat), now));
    assert!(!filter.admit(&app, &key(KeyCode::Enter, KeyEventKind::Repeat), now));
}

/// A letter held outside the composer is an action key there, so it does not
/// repeat.
#[test]
fn test_letter_repeat_outside_composer() {
    let mut filter = KeyFilter::default();
    let mut app = app();
    app.ui.navigation.viewport = ViewportMode::Focus;
    let now = Instant::now();
    assert!(!filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now));

    app.ui.navigation.viewport = ViewportMode::Working;
    app.ui.navigation.pane = Pane::Permission;
    assert!(!filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now));
}

/// Two presses for one non-BMP character arrive together with nothing
/// between them; the second belongs to the same character, not to a second
/// one.
#[test]
fn test_duplicate_press_dropped() {
    let mut filter = KeyFilter::default();
    let app = app();
    let emoji = KeyCode::Char('\u{1F600}');
    let start = Instant::now();
    assert!(filter.admit(&app, &key(emoji, KeyEventKind::Press), start));
    assert!(!filter.admit(
        &app,
        &key(emoji, KeyEventKind::Press),
        start + Duration::from_millis(2)
    ));
}

/// Two real presses of the same character 30ms apart are two characters, so
/// neither is dropped.
#[test]
fn test_real_double_press_kept() {
    let mut filter = KeyFilter::default();
    let app = app();
    let emoji = KeyCode::Char('\u{1F600}');
    let start = Instant::now();
    assert!(filter.admit(&app, &key(emoji, KeyEventKind::Press), start));
    assert!(filter.admit(
        &app,
        &key(emoji, KeyEventKind::Press),
        start + Duration::from_millis(30)
    ));
}

/// A held character repeats in every text field, not only the composer. Each
/// field is listed so one cannot silently lose its repeat when another is
/// added.
#[test]
fn test_repeat_types_in_fields() {
    let now = Instant::now();
    let mut filter = KeyFilter::default();
    let mut app = app();
    app.ui.transcript_view.begin_search_input();
    assert!(
        filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now),
        "the search bar types"
    );

    app.ui.transcript_view.cancel_search_input();
    app.enter_status_name_edit();
    assert!(
        filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now),
        "the status name editor types"
    );

    app.cancel_status_name_edit();
    app.ui.permission_pane.input = PermissionInput::Add;
    assert!(
        filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now),
        "the permissions rule editor types"
    );

    app.ui.permission_pane.input = PermissionInput::None;
    app.ui.navigation.pane = Pane::Artifact;
    app.workflow.artifact.enter_replace();
    assert!(
        filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now),
        "the artifact editor types"
    );

    // The memory pane keeps its text filter in the input box; letters edit
    // that filter while the box holds text and fall back to pane shortcuts
    // only when it is empty.
    app.ui.navigation.pane = Pane::Memory;
    app.ui.input.editor.set("cache".to_string());
    assert!(
        filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now),
        "the memory pane's text filter types"
    );
}

/// The question card's Other row is a text field once the cursor rests on it,
/// so a held character repeats there like any other input.
#[test]
fn test_question_other_repeats() {
    let now = Instant::now();
    let mut filter = KeyFilter::default();
    let mut app = app();
    let question = AskQuestion::parse(
        "call-1",
        &json!({ "questions": [{
            "question": "Pick one",
            "header": "Pick",
            "options": [
                { "label": "alpha", "description": "first" },
                { "label": "beta", "description": "second" }
            ],
            "multiSelect": false
        }] }),
    )
    .expect("parse card");
    app.sessions.current.prompt = Some(PendingPrompt::question_card(RequestId(0), question));
    let question = app.ask_question_mut().expect("the card is up");
    question.cursors[0] = question.current_other_idx();
    assert!(
        filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now),
        "the Other row types"
    );
}

/// A held navigation key repeats while an approval or question card is up:
/// moving through the rows of a decision is what the key is for.
#[test]
fn test_card_navigation_repeats() {
    let now = Instant::now();
    let mut filter = KeyFilter::default();
    let mut app = app();
    let question = AskQuestion::parse(
        "call-1",
        &json!({ "questions": [{
            "question": "Pick one",
            "header": "Pick",
            "options": [
                { "label": "alpha", "description": "first" },
                { "label": "beta", "description": "second" }
            ],
            "multiSelect": false
        }] }),
    )
    .expect("parse card");
    app.sessions.current.prompt = Some(PendingPrompt::question_card(RequestId(0), question));
    for code in [KeyCode::Down, KeyCode::Up, KeyCode::Tab] {
        assert!(
            filter.admit(&app, &key(code, KeyEventKind::Repeat), now),
            "{code:?} repeats while a card is up"
        );
    }
    assert!(
        !filter.admit(&app, &key(KeyCode::Char('h'), KeyEventKind::Repeat), now),
        "a letter on the option list is not text"
    );
}

/// A printable AltGr character keeps its repeat: it types like any other
/// character even though it carries control and alt.
#[test]
fn test_altgr_repeat_kept() {
    let mut filter = KeyFilter::default();
    let app = app();
    let now = Instant::now();
    let alt_gr = KeyEvent::new_with_kind(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyEventKind::Repeat,
    );
    assert!(
        filter.admit(&app, &alt_gr, now),
        "a printable AltGr character repeats"
    );
}

/// A held kill or background key must fire once: with a running child and an
/// empty box, its repeat would act on the next child.
#[test]
fn test_child_action_repeat_blocked() {
    let now = Instant::now();
    let mut filter = KeyFilter::default();
    let mut app = app();
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "child-0".into(),
            subagent_type: "explore".into(),
            description: String::new(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            outcome: None,
            completed_at: None,
            started_at: None,
        });
    for c in ['k', 'K', 'b'] {
        assert!(
            !filter.admit(&app, &key(KeyCode::Char(c), KeyEventKind::Repeat), now),
            "{c} must fire once, not once per repeat"
        );
    }
}

/// The AltGr form of a child-action letter is still that action, so its repeat
/// takes the same one-shot guard as the unmodified letter.
#[test]
fn test_altgr_child_action_blocked() {
    let now = Instant::now();
    let mut filter = KeyFilter::default();
    let mut app = app();
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "child-0".into(),
            subagent_type: "explore".into(),
            description: String::new(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            outcome: None,
            completed_at: None,
            started_at: None,
        });
    let alt_gr_kill = KeyEvent::new_with_kind(
        KeyCode::Char('k'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyEventKind::Repeat,
    );
    assert!(
        !filter.admit(&app, &alt_gr_kill, now),
        "the AltGr kill key must fire once, not once per repeat"
    );
}

/// The release between two presses makes the second one real even when it
/// arrives inside the duplicate window.
#[test]
fn test_release_clears_duplicate() {
    let mut filter = KeyFilter::default();
    let app = app();
    let emoji = KeyCode::Char('\u{1F600}');
    let start = Instant::now();
    assert!(filter.admit(&app, &key(emoji, KeyEventKind::Press), start));
    assert!(!filter.admit(&app, &key(emoji, KeyEventKind::Release), start));
    assert!(filter.admit(
        &app,
        &key(emoji, KeyEventKind::Press),
        start + Duration::from_millis(2)
    ));
}
