//! Real-binary PTY journey for the dollar input mode: a typed command leaves
//! the input box, runs in the host sandbox with no model turn, and its result
//! comes back into the transcript.
//!
//! Run via make suite ui (builds the bin first) or
//! cargo test --test ui_all ui_shell:: -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

use crate::common::{Key, PtySession, RENDER_TIMEOUT, pty_session, run_slash_command};
use std::thread;
use std::time::{Duration, Instant};

/// Poll the rendered screen until the predicate holds, so a negative
/// assertion can wait for the state it excludes to be gone rather than
/// reading one frame and hoping the host already answered.
fn screen_settles(s: &mut PtySession, mut done: impl FnMut(&str) -> bool) -> bool {
    let deadline = Instant::now() + RENDER_TIMEOUT;
    loop {
        if done(&s.screen().contents()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The status row advertises a direct shell command only while one is running,
/// so its absence is the end-of-command marker.
fn shell_row_cleared(screen: &str) -> bool {
    !screen.contains("shell running") && !screen.contains("shell stopping")
}

/// A command typed after the dollar prompt runs without a model request. The
/// awaited text is produced by the command, not typed into it, so seeing it
/// can only mean the command executed and its output reached the transcript;
/// the launch uses the stub provider, so a model turn would instead show its
/// stub reply. A finished call folds to one summary line, so the expansion key
/// opens the rows carrying the output.
#[test]
#[ignore]
fn test_dollar_command_runs() {
    let mut s = pty_session();
    s.send_str("$ printf 'shell-probe-%s' 4242");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("Ran 1 shell command", RENDER_TIMEOUT),
        "the command answers and folds into one summary line:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_screen("shell-probe-4242", RENDER_TIMEOUT),
        "the expanded rows carry the command's own output:\n{}",
        s.screen().contents()
    );
    assert!(
        screen_settles(&mut s, shell_row_cleared),
        "the status row stops advertising the finished command:\n{}",
        s.screen().contents()
    );
    assert!(
        !s.output().contains("stub mode: no api key"),
        "the command must not start a model turn:\n{}",
        s.output()
    );
}

/// Typing the start of a command already run shows the rest as ghost text,
/// and Tab takes it into the box without running anything.
///
/// The first run's transcript is archived before the second starts, so the
/// summary line and the expanded output the assertion waits for can only come
/// from the completed second command: the first run's identical output is no
/// longer on screen to satisfy it.
#[test]
#[ignore]
fn test_tab_takes_ghost() {
    let mut s = pty_session();
    s.send_str("$ printf 'ghost-%s' full-tail");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("Ran 1 shell command", RENDER_TIMEOUT),
        "the first run records the command in the shell history:\n{}",
        s.screen().contents()
    );
    // The shell history outlives the transcript, so the archive keeps the
    // completion source while removing every row the first run drew.
    run_slash_command(&mut s, "clear");
    assert!(
        s.wait_for_screen("session archived, new session started", RENDER_TIMEOUT),
        "clear archives the first run's transcript:\n{}",
        s.screen().contents()
    );
    s.send_str("$ printf 'ghost-%s' f");
    // The cursor cell sits between the typed prefix and the suffix, so the
    // suffix is matched on its own; the run below is what proves the ghost
    // completed the command.
    assert!(
        s.wait_for_screen("ull-tail", RENDER_TIMEOUT),
        "the history offers the rest of the command as ghost text:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Tab);
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("Ran 1 shell command", RENDER_TIMEOUT),
        "the completed command runs:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_screen("ghost-full-tail", RENDER_TIMEOUT),
        "the output comes from the completed command, not the prefix:\n{}",
        s.screen().contents()
    );
}

/// Ctrl+C stops a running command by the same rule as Esc: the interrupt key
/// must reach the command when no model turn owns the run, and the transcript
/// records the interruption rather than watching output that never arrives.
#[test]
#[ignore]
fn test_ctrl_c_stops_shell() {
    let mut s = pty_session();
    s.send_str("$ sleep 30");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("shell running", RENDER_TIMEOUT),
        "the status row advertises the running command:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Ctrl('c'));
    assert!(
        screen_settles(&mut s, shell_row_cleared),
        "the status row clears once the host answers the stop:\n{}",
        s.screen().contents()
    );
    assert!(
        screen_settles(&mut s, |screen| {
            screen.contains("interrupted by user") && !screen.contains("Ran 1 shell command")
        }),
        "the transcript names the interruption and counts no run:\n{}",
        s.screen().contents()
    );
}

/// Esc stops a command that is still running: the transcript records the
/// interruption instead of waiting for output that will never arrive, and the
/// status row clears itself once the host answers the stop.
#[test]
#[ignore]
fn test_esc_stops_dollar_command() {
    let mut s = pty_session();
    s.send_str("$ sleep 30");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("shell running", RENDER_TIMEOUT),
        "the status row advertises the running command:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Esc);
    assert!(
        screen_settles(&mut s, shell_row_cleared),
        "the status row clears once the host answers the stop:\n{}",
        s.screen().contents()
    );
    // A stopped command is not counted as run: its rows stay visible on their
    // own, so the interruption reads without an expansion key and no summary
    // claims the command executed.
    assert!(
        screen_settles(&mut s, |screen| {
            screen.contains("interrupted by user") && !screen.contains("Ran 1 shell command")
        }),
        "the transcript names the interruption and counts no run:\n{}",
        s.screen().contents()
    );
}
