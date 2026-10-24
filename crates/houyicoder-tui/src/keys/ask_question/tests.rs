//! The question card's own key transitions.
//!
//! The card answers with a decision instead of acting on the shell, so its
//! state machine is exercised directly here: what each key does to the
//! selection, the question index, and the verdict it produces.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

use super::AskDecision;
use crate::ask_question_model::AskQuestion;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn card(questions: serde_json::Value) -> AskQuestion {
    AskQuestion::parse("call-1", &json!({ "questions": questions })).expect("parse card")
}

fn single() -> AskQuestion {
    card(json!([{
        "question": "Pick one",
        "header": "Pick",
        "options": [
            { "label": "alpha", "description": "first" },
            { "label": "beta", "description": "second" }
        ],
        "multiSelect": false
    }]))
}

fn two_questions() -> AskQuestion {
    card(json!([
        {
            "question": "First",
            "header": "One",
            "options": [
                { "label": "alpha", "description": "a" },
                { "label": "beta", "description": "b" }
            ],
            "multiSelect": false
        },
        {
            "question": "Second",
            "header": "Two",
            "options": [
                { "label": "gamma", "description": "c" },
                { "label": "delta", "description": "d" }
            ],
            "multiSelect": false
        }
    ]))
}

/// Moving down and up walks the option list and wraps at both ends. The row
/// count includes the Other entry the card always offers.
#[test]
fn test_option_cursor_walks() {
    let mut c = single();
    let rows = c.current_option_count();
    assert_eq!(c.cursors[0], 0);
    for expected in 1..rows {
        assert!(matches!(
            c.handle_key(key(KeyCode::Down)),
            AskDecision::None
        ));
        assert_eq!(c.cursors[0], expected, "down moves one row");
    }
    assert!(matches!(
        c.handle_key(key(KeyCode::Down)),
        AskDecision::None
    ));
    assert_eq!(c.cursors[0], 0, "down wraps to the first row");
    assert!(matches!(c.handle_key(key(KeyCode::Up)), AskDecision::None));
    assert_eq!(c.cursors[0], rows - 1, "up wraps to the last row");
}

/// Tab and Right advance the question; BackTab and Left return, clamped.
/// The index range includes the review step past the last question, and the
/// review step consumes Left/Right itself to move between Submit and Cancel.
#[test]
fn test_question_navigation() {
    let mut c = two_questions();
    let last = c.max_index();
    assert_eq!(c.current, 0);
    assert!(matches!(c.handle_key(key(KeyCode::Tab)), AskDecision::None));
    assert_eq!(c.current, 1, "tab advances");
    assert!(matches!(
        c.handle_key(key(KeyCode::Left)),
        AskDecision::None
    ));
    assert_eq!(c.current, 0, "left returns");
    assert!(matches!(
        c.handle_key(key(KeyCode::Left)),
        AskDecision::None
    ));
    assert_eq!(c.current, 0, "left stops at the first question");
    for expected in 1..=last {
        assert!(matches!(
            c.handle_key(key(KeyCode::Right)),
            AskDecision::None
        ));
        assert_eq!(c.current, expected, "right advances");
    }
    assert!(matches!(
        c.handle_key(key(KeyCode::Right)),
        AskDecision::None
    ));
    assert_eq!(c.current, last, "right stops at the last step");
}

/// A single single-select question submits as soon as its option is chosen.
#[test]
fn test_single_question_submits() {
    let mut c = single();
    let decision = c.handle_key(key(KeyCode::Enter));
    let AskDecision::Resolve(verdict) = decision else {
        panic!("a single question submits on Enter");
    };
    assert!(verdict.approved, "choosing an option approves");
    assert_eq!(verdict.call_id, "call-1");
    assert_eq!(verdict.scope, "once");
    let input = verdict.updated_input.expect("answers ride the verdict");
    assert!(
        input.to_string().contains("alpha"),
        "the chosen label is in the answers: {input}"
    );
}

/// With two questions the first Enter records the answer and advances; the
/// second reaches the review step, where Enter submits.
#[test]
fn test_two_questions_submit() {
    let mut c = two_questions();
    assert!(matches!(
        c.handle_key(key(KeyCode::Enter)),
        AskDecision::None
    ));
    assert_eq!(
        c.current, 1,
        "the first answer advances to the next question"
    );
    assert!(matches!(
        c.handle_key(key(KeyCode::Enter)),
        AskDecision::None
    ));
    assert!(c.is_submit_view(), "the last answer opens the review");
    let decision = c.handle_key(key(KeyCode::Enter));
    let AskDecision::Resolve(verdict) = decision else {
        panic!("Enter on the review submits");
    };
    let text = verdict.updated_input.expect("answers").to_string();
    assert!(text.contains("alpha"), "first answer kept: {text}");
    assert!(text.contains("gamma"), "second answer kept: {text}");
}

/// Esc declines the card rather than rejecting it: the tool has to be able to
/// tell "the user would not answer" from "the user vetoed the call".
#[test]
fn test_esc_declines() {
    let mut c = single();
    let decision = c.handle_key(key(KeyCode::Esc));
    let AskDecision::Resolve(verdict) = decision else {
        panic!("Esc produces a verdict");
    };
    assert!(verdict.approved, "a decline is not a rejection of the call");
    let input = verdict.updated_input.expect("declined input");
    assert_eq!(input["declined"], json!(true), "the declined flag is set");
}

/// Enter on the Other row opens the text input; typing lands there and Enter
/// carries the typed text into the answer.
#[test]
fn test_other_text_carried() {
    let mut c = single();
    let other_idx = c.current_other_idx();
    c.cursors[0] = other_idx;
    assert!(matches!(
        c.handle_key(key(KeyCode::Enter)),
        AskDecision::None
    ));
    assert!(c.other_focused, "Enter on Other focuses the text input");
    for ch in "custom".chars() {
        assert!(matches!(
            c.handle_key(key(KeyCode::Char(ch))),
            AskDecision::None
        ));
    }
    let decision = c.handle_key(key(KeyCode::Enter));
    let AskDecision::Resolve(verdict) = decision else {
        panic!("Enter in the text input submits a single question");
    };
    let text = verdict.updated_input.expect("answers").to_string();
    assert!(
        text.contains("custom"),
        "the typed text is the answer: {text}"
    );
}

/// Esc in the text input returns to the option list without a verdict.
#[test]
fn test_other_esc_returns() {
    let mut c = single();
    let other_idx = c.current_other_idx();
    c.cursors[0] = other_idx;
    drop(c.handle_key(key(KeyCode::Enter)));
    assert!(c.other_focused);
    assert!(matches!(c.handle_key(key(KeyCode::Esc)), AskDecision::None));
    assert!(!c.other_focused, "Esc leaves the text input");
}

/// Backspace removes the last typed character and never panics on an empty
/// entry.
#[test]
fn test_other_backspace() {
    let mut c = single();
    let other_idx = c.current_other_idx();
    c.cursors[0] = other_idx;
    drop(c.handle_key(key(KeyCode::Enter)));
    drop(c.handle_key(key(KeyCode::Char('x'))));
    drop(c.handle_key(key(KeyCode::Backspace)));
    assert_eq!(c.other_text[0].as_deref(), Some(""));
    // A second backspace on the empty entry is a no-op, not a panic.
    assert!(matches!(
        c.handle_key(key(KeyCode::Backspace)),
        AskDecision::None
    ));
}

/// The submit view toggles between Submit and Cancel and acts on the choice.
#[test]
fn test_submit_view_cancel_choice() {
    let mut c = two_questions();
    // Answer both questions to reach the submit view.
    drop(c.handle_key(key(KeyCode::Enter)));
    drop(c.handle_key(key(KeyCode::Enter)));
    assert!(
        c.is_submit_view(),
        "answering every question opens the review"
    );
    // The review opens on Submit; one move selects Cancel.
    drop(c.handle_key(key(KeyCode::Down)));
    let decision = c.handle_key(key(KeyCode::Enter));
    let AskDecision::Resolve(verdict) = decision else {
        panic!("the review produces a verdict");
    };
    assert_eq!(
        verdict.updated_input.expect("declined input")["declined"],
        json!(true),
        "choosing Cancel declines"
    );
}
