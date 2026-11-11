//! Key lifecycle and paste-burst behavior seen through the event entry point:
//! which events act, which are ignored, and where a folded paste ends up.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::handle_event;
use crate::input::paste::burst::coalesce_with;
use crate::keys::filter::KeyFilter;
use crate::records::TranscriptLine;
use crate::state::{App, Pane, StatusTab, ViewportMode};
use crate::test_harness::working_app;
use crate::todo_view::{TodoStatus, TodoView};

fn composer() -> App {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Transcript;
    app.ui.navigation.viewport = ViewportMode::Working;
    app
}

fn keyed(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
    KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
}

fn chord(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
    KeyEvent::new_with_kind(code, KeyModifiers::CONTROL, kind)
}

/// Feed one key event and report whether it asked for a redraw.
fn feed(app: &mut App, filter: &mut KeyFilter, key: KeyEvent) -> bool {
    handle_event(app, Event::Key(key), filter).expect("a key event carries no io")
}

/// Give the transcript a checklist so the fold chord has a block to act on.
fn with_checklist(app: &mut App) {
    app.sessions.current.todos.items.push(TodoView {
        content: "run the gate".to_string(),
        status: TodoStatus::Pending,
        active_form: None,
    });
}

/// A release never types and never asks for a frame.
#[test]
fn test_release_types_nothing() {
    let mut app = composer();
    let mut filter = KeyFilter::default();
    assert!(feed(
        &mut app,
        &mut filter,
        keyed(KeyCode::Char('h'), KeyEventKind::Press)
    ));
    assert_eq!(app.ui.input.editor.value(), "h");
    assert!(!feed(
        &mut app,
        &mut filter,
        keyed(KeyCode::Char('h'), KeyEventKind::Release)
    ));
    assert_eq!(app.ui.input.editor.value(), "h", "the release typed again");
}

/// A held arrow repeats through the pane it navigates, and its release adds
/// nothing.
#[test]
fn test_repeat_moves_cursor() {
    let mut app = composer();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    let mut filter = KeyFilter::default();
    feed(
        &mut app,
        &mut filter,
        keyed(KeyCode::Down, KeyEventKind::Press),
    );
    assert_eq!(app.ui.navigation.status_pane.config_cursor, Some(1));
    assert!(feed(
        &mut app,
        &mut filter,
        keyed(KeyCode::Down, KeyEventKind::Repeat)
    ));
    assert_eq!(
        app.ui.navigation.status_pane.config_cursor,
        Some(2),
        "a held arrow repeats instead of acting once"
    );
    assert!(!feed(
        &mut app,
        &mut filter,
        keyed(KeyCode::Down, KeyEventKind::Release)
    ));
    assert_eq!(app.ui.navigation.status_pane.config_cursor, Some(2));
}

/// The fold chord acts on its press alone: a held key must not toggle the
/// block back.
#[test]
fn test_ctrl_o_repeat_folds() {
    let mut app = composer();
    with_checklist(&mut app);
    let mut filter = KeyFilter::default();
    assert!(feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('o'), KeyEventKind::Press)
    ));
    assert!(app.sessions.current.todos.expanded);
    assert!(!feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('o'), KeyEventKind::Repeat)
    ));
    assert!(
        app.sessions.current.todos.expanded,
        "a held fold chord must not fold back"
    );
    assert!(!feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('o'), KeyEventKind::Release)
    ));
    assert!(app.sessions.current.todos.expanded);
}

/// The exit chord needs a second deliberate press; the repeat of the first
/// press is not one.
#[test]
fn test_ctrl_d_repeat_ignored() {
    let mut app = composer();
    let mut filter = KeyFilter::default();
    feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('d'), KeyEventKind::Press),
    );
    assert!(!app.ui.quit, "one ctrl+D must not quit");
    assert!(
        app.ui
            .notifications
            .current()
            .is_some_and(|n| n.key == "exit-confirm"),
        "the first ctrl+D asks for confirmation"
    );
    assert!(!feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('d'), KeyEventKind::Repeat)
    ));
    assert!(
        !app.ui.quit,
        "the repeat of the first press is not a second one"
    );
    assert!(!feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('d'), KeyEventKind::Release)
    ));
    assert!(!app.ui.quit);
    // A second deliberate press is the confirmation.
    feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('d'), KeyEventKind::Press),
    );
    assert!(app.ui.quit, "the second press quits");
}

/// The fold chord works while the reader is scrolled back and keeps them
/// there.
#[test]
fn test_ctrl_o_keeps_scroll() {
    let mut app = composer();
    with_checklist(&mut app);
    app.enter_scroll();
    let mut filter = KeyFilter::default();
    assert!(feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('o'), KeyEventKind::Press)
    ));
    assert_eq!(app.ui.navigation.viewport, ViewportMode::Scroll);
    assert!(app.sessions.current.todos.expanded);
}

/// The fold chord works over the search results and does not close the view.
#[test]
fn test_ctrl_o_keeps_search() {
    let mut app = composer();
    with_checklist(&mut app);
    app.enter_search_view("needle");
    assert!(app.ui.transcript_view.search_active());
    let mut filter = KeyFilter::default();
    assert!(feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('o'), KeyEventKind::Press)
    ));
    assert_eq!(app.ui.navigation.viewport, ViewportMode::Scroll);
    assert!(
        app.ui.transcript_view.search_active(),
        "folding must not close the search view"
    );
    assert!(app.sessions.current.todos.expanded);
}

/// While the search box owns the keyboard, the chord is text for the box and
/// no block folds.
#[test]
fn test_ctrl_o_search_bar() {
    let mut app = composer();
    with_checklist(&mut app);
    app.enter_search_view("needle");
    app.enter_search_input();
    assert!(app.ui.transcript_view.search_input_mode());
    let mut filter = KeyFilter::default();
    feed(
        &mut app,
        &mut filter,
        chord(KeyCode::Char('o'), KeyEventKind::Press),
    );
    assert!(
        !app.sessions.current.todos.expanded,
        "the chord belongs to the search box, not to the folds"
    );
    assert!(app.ui.transcript_view.search_input_mode());
}

/// A folded paste that carries line breaks never submits them; the whole run
/// lands in the input box as one placeholder.
#[test]
fn test_paste_never_submits() {
    let mut app = composer();
    let mut filter = KeyFilter::default();
    let mut queue: VecDeque<Event> = "one\ntwo\nthree\nfour"
        .chars()
        .map(|c| {
            let code = if c == '\n' {
                KeyCode::Enter
            } else {
                KeyCode::Char(c)
            };
            Event::Key(keyed(code, KeyEventKind::Press))
        })
        .collect();
    let first = queue.pop_front().expect("the run has a first key");
    let queue = Rc::new(RefCell::new(queue));
    let pending = {
        let queue = Rc::clone(&queue);
        move |_wait: Duration| !queue.borrow().is_empty()
    };
    let read = {
        let queue = Rc::clone(&queue);
        move || Ok(queue.borrow_mut().pop_front().expect("a queued key"))
    };
    let events = coalesce_with(first, pending, read);
    assert_eq!(events.len(), 1, "the run folds into one event");
    for event in events {
        handle_event(&mut app, event, &mut filter).expect("a paste carries no io");
    }
    let submitted = app
        .sessions
        .current
        .transcript
        .iter()
        .any(|line| matches!(line, TranscriptLine::User(_)));
    assert!(!submitted, "no line of the paste submitted the draft");
    // The flush waits out the paste gap, so move the clock past it.
    app.ui.input.pending_paste.last = Some(Instant::now() - Duration::from_millis(100));
    assert!(app.flush_pending_paste());
    assert_eq!(
        app.ui.input.editor.value(),
        "[Pasted text #1 +3 lines]",
        "the whole run becomes one placeholder"
    );
}

/// A key pressed inside the paste gap routes the buffered text first, so the
/// key acts on a draft that already holds the paste instead of on an empty
/// box.
#[test]
fn test_key_flushes_paste() {
    let mut app = composer();
    let mut filter = KeyFilter::default();
    handle_event(&mut app, Event::Paste("hello".into()), &mut filter)
        .expect("a paste carries no io");
    assert!(
        app.ui.input.pending_paste.buffer.is_some(),
        "the paste is still waiting out its gap"
    );
    assert!(feed(
        &mut app,
        &mut filter,
        keyed(KeyCode::Enter, KeyEventKind::Press)
    ));
    let submitted = app
        .sessions
        .current
        .transcript
        .iter()
        .any(|line| matches!(line, TranscriptLine::User(text) if text == "hello"));
    assert!(
        submitted,
        "the pasted text reaches the draft before the key acts"
    );
}

/// A single Enter on a draft submits it exactly once.
#[test]
fn test_enter_submits_once() {
    let mut app = composer();
    let mut filter = KeyFilter::default();
    app.ui.input.editor.set("hello".to_string());
    assert!(feed(
        &mut app,
        &mut filter,
        keyed(KeyCode::Enter, KeyEventKind::Press)
    ));
    let submitted = app
        .sessions
        .current
        .transcript
        .iter()
        .filter(|line| matches!(line, TranscriptLine::User(text) if text == "hello"))
        .count();
    assert_eq!(submitted, 1);
    assert!(app.ui.input.editor.value().is_empty(), "the draft is taken");
}
