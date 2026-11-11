//! Real-binary PTY smoke for the exit-key alignment. #[ignore] (spawns
//! the binary + a PTY). Run via make suite ui or
//! cargo test --test ui_all ui_exit_keys:: -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

use crate::common::{Key, RENDER_TIMEOUT, pty_session};

/// A single ctrl+D shows the exit-confirm toast and does not quit (the
/// toast rendering proves the app is alive + processed the key).
#[test]
#[ignore]
fn test_ctrl_d_once_toast() {
    let mut s = pty_session();
    s.send_key(&Key::Ctrl('d'));
    assert!(
        s.wait_for_compact("PressCtrl+Dagaintoexit", RENDER_TIMEOUT),
        "the exit-confirm toast should render on the first ctrl+D:\n{}",
        s.output()
    );
    drop(s);
}

/// ctrl+C with an empty box (no selection, no run) is a no-op: the app
/// stays alive, so a following ctrl+D still shows the toast.
#[test]
#[ignore]
fn test_ctrl_c_idle_noop() {
    let mut s = pty_session();
    s.clear_output();
    s.send_key(&Key::Ctrl('c'));
    s.send_key(&Key::Ctrl('d'));
    assert!(
        s.wait_for_compact("PressCtrl+Dagaintoexit", RENDER_TIMEOUT),
        "ctrl+C idle must not quit -- a following ctrl+D should still show the toast:\n{}",
        s.output()
    );
    drop(s);
}

/// q with an empty box types instead of quitting: backspace clears it back
/// to the placeholder (the app is alive to process the backspace).
///
/// The two keys are typed into one input burst, which the app drains and
/// draws once, so the pair leaves the box exactly as it found it and paints
/// no bytes for the edit at all. A later character is the observable that
/// tells the two outcomes apart: it shows on a live app, and with the q left
/// in the box it would follow that q.
#[test]
#[ignore]
fn test_q_empty_types() {
    let mut s = pty_session();
    s.clear_output();
    s.send_key(&Key::Char('q'));
    s.send_key(&Key::Backspace);
    s.send_str("alive");
    assert!(
        s.wait_for_plain("alive", RENDER_TIMEOUT),
        "q should type (not quit); backspace should clear the box:\n{}",
        s.output()
    );
    assert!(
        !s.screen().contents().contains("qalive"),
        "backspace should clear the typed q before the next text:\n{}",
        s.screen().contents()
    );
    drop(s);
}
