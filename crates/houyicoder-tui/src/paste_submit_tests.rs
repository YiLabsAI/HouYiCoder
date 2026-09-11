//! Pasted-text storage, expansion, and submission tests.

#![cfg(test)]

use crate::composition;
use crate::state::Screen;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

#[test]
fn test_paste_submit_expands() {
    let mut app = working();
    let big = "x".repeat(900);
    let token = app.pasted.ingest(&big);
    assert!(token.starts_with("[Pasted text #"));
    app.input.set(token);
    app.submit_input();
    // The echoed User line must be the expanded real text, not the token.
    let user = app.transcript.iter().find_map(|l| match l {
        crate::state::TranscriptLine::User(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(user.as_deref(), Some(big.as_str()));
    // Store is NOT cleared after submit — ids increment across the session
    // (entries persist for the session lifetime).
    assert!(!app.pasted.is_empty(), "store retains entries after submit");
}

#[test]
fn test_two_pastes_submit_distinct() {
    let mut app = working();
    let a = "first ".to_string() + &"a".repeat(900);
    let b = "second ".to_string() + &"b".repeat(900);
    // first paste + submit
    app.input.set(app.pasted.ingest(&a));
    app.submit_input();
    let first = app
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            crate::state::TranscriptLine::User(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(first, a);
    // second paste + submit — must send b, not a
    app.input.set(app.pasted.ingest(&b));
    app.submit_input();
    let second = app
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            crate::state::TranscriptLine::User(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(
        second, b,
        "second send must be the second paste, not the first"
    );
}
