//! Render + height tests for the AskUserQuestion card: nav bar tabs, the
//! multi-select Submit/Next button, and card-height accounting. Split from
//! ask_question_tests.rs to keep both files under the file-size gate. Uses the
//! shared render_text helper (buffer-level, not pixel snapshots).

use serde_json::json;

use crate::ask_question_tests::{multi_input, single_input, two_question_input};
use crate::records::AskQuestion;

// --- nav bar rendering ---

#[test]
fn test_render_nav_tabs() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("h1"), "header 1 tab missing:\n{out}");
    assert!(out.contains("h2"), "header 2 tab missing:\n{out}");
    assert!(out.contains("Submit"), "submit tab missing:\n{out}");
}

#[test]
fn test_render_nav_hidden_single() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    // No nav bar (no Submit tab, no h1/h2 tabs). The header chip [Library]
    // is still present.
    assert!(
        !out.contains("Submit"),
        "submit tab should be hidden:\n{out}"
    );
}

// --- multi-select submit button rendering ---

#[test]
fn test_render_multi_next_btn() {
    // Two multi-select questions: the first shows "Next", not "Submit".
    let input = json!({
        "questions": [
            {"question": "q1", "header": "h1", "options": [{"label":"a","description":"x"},{"label":"b","description":"y"}], "multiSelect": true},
            {"question": "q2", "header": "h2", "options": [{"label":"c","description":"x"},{"label":"d","description":"y"}], "multiSelect": true}
        ]
    });
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let aq = AskQuestion::parse("c1", &input).expect("parse");
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("Next"), "Next button missing:\n{out}");
}

#[test]
fn test_render_multi_submit_last() {
    let input = json!({
        "questions": [
            {"question": "q1", "header": "h1", "options": [{"label":"a","description":"x"},{"label":"b","description":"y"}], "multiSelect": true},
            {"question": "q2", "header": "h2", "options": [{"label":"c","description":"x"},{"label":"d","description":"y"}], "multiSelect": true}
        ]
    });
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let mut aq = AskQuestion::parse("c1", &input).expect("parse");
    aq.current = 1; // last question
    app.ask_question = Some(aq);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("Submit"), "Submit button missing:\n{out}");
    assert!(
        !out.contains("Next"),
        "Next should not appear on last:\n{out}"
    );
}

// --- card height ---

#[test]
fn test_height_single_question() {
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse");
    // 3 opts (2+Other) + 6 base + 0 nav + 0 submit_btn = 9
    assert_eq!(aq.card_height(), 9);
}

#[test]
fn test_height_two_questions() {
    let aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    // 3 opts + 6 base + 1 nav + 0 submit_btn = 10
    assert_eq!(aq.card_height(), 10);
}

#[test]
fn test_height_multi_select() {
    let aq = AskQuestion::parse("c1", &multi_input()).expect("parse");
    // 4 opts (3+Other) + 6 base + 1 nav + 1 submit_btn = 12
    assert_eq!(aq.card_height(), 12);
}

#[test]
fn test_height_submit_view() {
    let mut aq = AskQuestion::parse("c1", &two_question_input()).expect("parse");
    aq.current = aq.questions.len(); // submit view
    // 2 questions + 8 base = 10
    assert_eq!(aq.card_height(), 10);
}
