use std::cell::RefCell;
use std::io::{Error, Result};
use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use super::{BURST_GAP, coalesce_with};

fn key(code: KeyCode, kind: KeyEventKind) -> Event {
    Event::Key(KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind))
}

fn press(code: KeyCode) -> Event {
    key(code, KeyEventKind::Press)
}

fn release(code: KeyCode) -> Event {
    key(code, KeyEventKind::Release)
}

/// Drive a collection over a queue that holds every record after the first.
fn burst(first: Event, rest: Vec<Event>) -> Vec<Event> {
    let queue = Rc::new(RefCell::new(rest));
    let pending = Rc::clone(&queue);
    let read = Rc::clone(&queue);
    coalesce_with(
        first,
        move |_: Duration| !pending.borrow().is_empty(),
        move || -> Result<Event> {
            let mut pending = read.borrow_mut();
            if pending.is_empty() {
                Err(Error::other("burst ended"))
            } else {
                Ok(pending.remove(0))
            }
        },
    )
}

/// A run of characters with a newline in it is one paste, so the newline never
/// reaches the editor as a separate Enter that submits the draft.
#[test]
fn test_burst_folds_multiline() {
    let rest = vec![
        release(KeyCode::Char('a')),
        press(KeyCode::Char('b')),
        release(KeyCode::Char('b')),
        press(KeyCode::Enter),
        release(KeyCode::Enter),
        press(KeyCode::Char('c')),
        release(KeyCode::Char('c')),
    ];
    let folded = burst(press(KeyCode::Char('a')), rest);
    assert_eq!(folded, vec![Event::Paste("ab\nc".to_string())]);
}

/// A single Enter with nothing queued behind it is delivered as the key it is,
/// so the ordinary submit path is untouched.
#[test]
fn test_single_enter_passes_through() {
    let mut consulted = 0;
    let folded = coalesce_with(
        press(KeyCode::Enter),
        |_| {
            consulted += 1;
            false
        },
        || panic!("no record is read when the queue is empty"),
    );
    assert_eq!(folded.len(), 1);
    assert!(
        matches!(&folded[0], Event::Key(k) if k.code == KeyCode::Enter),
        "the Enter stays a key event"
    );
    assert_eq!(consulted, 1, "the queue was consulted once");
}

/// A character whose release was already queued is not a paste: one typed
/// character stays a typed character even when the loop was busy.
#[test]
fn test_single_key_stays_typed() {
    let folded = burst(press(KeyCode::Char('a')), vec![release(KeyCode::Char('a'))]);
    assert_eq!(
        folded,
        vec![press(KeyCode::Char('a')), release(KeyCode::Char('a'))]
    );
}

/// The terminal's paste markers ride along as ordinary keys and are removed
/// from the folded text.
#[test]
fn test_burst_strips_paste_markers() {
    let mut rest = vec![press(KeyCode::Char('['))];
    for c in "200~x".chars() {
        rest.push(press(KeyCode::Char(c)));
    }
    rest.push(press(KeyCode::Esc));
    for c in "[201~".chars() {
        rest.push(press(KeyCode::Char(c)));
    }
    let folded = burst(press(KeyCode::Esc), rest);
    assert_eq!(folded, vec![Event::Paste("x".to_string())]);
}

/// A run that reaches a key the paste path cannot carry is handed back whole,
/// so a chord typed right after a character still performs its action.
#[test]
fn test_burst_stops_on_chord() {
    let control = |code| {
        Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ))
    };
    let folded = burst(
        press(KeyCode::Char('a')),
        vec![release(KeyCode::Char('a')), control(KeyCode::Char('c'))],
    );
    assert_eq!(folded.len(), 3, "nothing is folded when a chord is present");
    assert!(matches!(&folded[2], Event::Key(k) if k.modifiers == KeyModifiers::CONTROL));
}

/// One character above the basic plane arrives as two presses with no release
/// between them; the second press is not a second character.
#[test]
fn test_burst_drops_mirrored_emoji() {
    let emoji = '\u{1F600}';
    let folded = burst(
        press(KeyCode::Char(emoji)),
        vec![
            press(KeyCode::Char(emoji)),
            press(KeyCode::Char('a')),
            release(KeyCode::Char('a')),
        ],
    );
    assert_eq!(folded, vec![Event::Paste("\u{1F600}a".to_string())]);
}

/// A typed key never pays the burst gap: the records the console already holds
/// are taken at once and the run is handed back, so a key pressed right after
/// it keeps its own meaning instead of being folded into a paste.
#[test]
fn test_single_key_skips_gap() {
    let windows = Rc::new(RefCell::new(Vec::new()));
    let queue = Rc::new(RefCell::new(vec![release(KeyCode::Char('a'))]));
    let recorded = Rc::clone(&windows);
    let pending_queue = Rc::clone(&queue);
    let read_queue = Rc::clone(&queue);
    let folded = coalesce_with(
        press(KeyCode::Char('a')),
        move |window| {
            recorded.borrow_mut().push(window);
            !pending_queue.borrow().is_empty()
        },
        move || Ok(read_queue.borrow_mut().remove(0)),
    );
    assert_eq!(
        folded,
        vec![press(KeyCode::Char('a')), release(KeyCode::Char('a'))]
    );
    assert!(
        windows
            .borrow()
            .iter()
            .all(|window| *window == Duration::ZERO),
        "a typed key must not wait on the console: {:?}",
        windows.borrow()
    );
}

/// A mouse event that arrives during a paste is kept as its own event; it must
/// not break the run into raw keystrokes, which would turn every newline in the
/// paste into a submit.
#[test]
fn test_paste_keeps_mouse_events() {
    let mouse = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 4,
        row: 2,
        modifiers: KeyModifiers::NONE,
    });
    let folded = burst(
        press(KeyCode::Char('a')),
        vec![
            mouse.clone(),
            press(KeyCode::Char('b')),
            release(KeyCode::Char('b')),
        ],
    );
    assert_eq!(folded, vec![Event::Paste("ab".to_string()), mouse]);
}

/// A real shortcut at the end of a paste ends the text run: the text before it
/// is handed over as one paste and the shortcut still performs its action.
#[test]
fn test_multiline_ends_at_chord() {
    let control_c = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    ));
    let folded = burst(
        press(KeyCode::Char('a')),
        vec![
            release(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Enter),
            release(KeyCode::Enter),
            control_c.clone(),
        ],
    );
    assert_eq!(folded, vec![Event::Paste("ab\n".to_string()), control_c]);
}

/// A printable character typed with control and alt held is a layout's AltGr
/// form of that character, not a shortcut: it belongs in the pasted text.
#[test]
fn test_altgr_letter_is_text() {
    let alt_gr = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyEventKind::Press,
    ));
    let folded = burst(
        press(KeyCode::Char('a')),
        vec![release(KeyCode::Char('a')), alt_gr],
    );
    assert_eq!(folded, vec![Event::Paste("ae".to_string())]);
}

/// The gap wait exists for a run that is already a paste. Every wait a paste
/// pays is the burst gap, never zero.
#[test]
fn test_paste_waits_the_gap() {
    let windows = Rc::new(RefCell::new(Vec::new()));
    let queue = Rc::new(RefCell::new(vec![
        press(KeyCode::Char('b')),
        release(KeyCode::Char('b')),
    ]));
    let recorded = Rc::clone(&windows);
    let pending_queue = Rc::clone(&queue);
    let read_queue = Rc::clone(&queue);
    let folded = coalesce_with(
        press(KeyCode::Char('a')),
        move |window| {
            recorded.borrow_mut().push(window);
            !pending_queue.borrow().is_empty()
        },
        move || Ok(read_queue.borrow_mut().remove(0)),
    );
    assert_eq!(folded, vec![Event::Paste("ab".to_string())]);
    assert!(
        windows.borrow().contains(&BURST_GAP),
        "a paste collects within the gap: {:?}",
        windows.borrow()
    );
}
