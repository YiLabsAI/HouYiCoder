//! Real-binary PTY tests for input-box key behavior: the busy-Esc gate (#15)
//! and the readline-style clear shortcuts (Ctrl+U kill-to-line-start, idle Esc
//! clear). The unit layer covers InputBuffer mutation + the keys.rs gate; this
//! layer drives the real crossterm byte path so the key-routing + repaint chain
//! is pinned, not just the state machine.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_input -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{
    Key, RENDER_TIMEOUT, session_on_working, session_on_working_slow_in_repo,
    session_on_working_slow_with_script,
};
use std::path::PathBuf;
use std::process::Command;

/// Seed a throwaway git repo for isolated PTY startup (see common/mod.rs).
#[allow(clippy::disallowed_methods)]
fn make_temp_repo(slug: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-input-repo-{}-{slug}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@x"][..],
        &["config", "user.name", "t"][..],
        &["add", "Cargo.toml"][..],
        &["commit", "-m", "init", "-q"][..],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {:?}", args);
    }
    dir
}
use std::time::Duration;

/// Large enough that the stub's first delta lands well after the test's key
/// sequence, so the run is in-flight with zero streamed content for the whole
/// test (the busy-Esc gate is exercised before any content arrives).
const RUN_DELAY_MS: u64 = 3000;

/// A token that never appears elsewhere in the render (status bar, prompts, the
/// stub reply) so its presence as a contiguous user echo cleanly proves a
/// submit happened, and its absence proves the input was cleared before Enter.
const UNIQUE_TOKEN: &str = "zzqxwaffle";

/// The queued message for the Esc-pop test: distinct from the first run's
/// input ("first task") so a submit of the WRONG text (the interrupt-restore
/// path re-filling the box with the aborted run's origin) fails the wait.
const QUEUED_TOKEN: &str = "zzqueuedpony";

/// Esc while a run is in-flight with a draft aborts the run AND leaves the
/// draft intact (so the user can resend after redirecting). This is the
/// property the interrupt/recall split buys: a panic Esc never destroys the
/// user's half-typed input. The earlier clear-draft-on-first-Esc gate was
/// removed because it made "stop the run" require first destroying the
/// draft — a panic key must not force data loss as its first step.
#[test]
#[ignore]
fn test_esc_draft_aborts_kept() {
    let mut s = session_on_working_slow_with_script(
        RUN_DELAY_MS,
        r#"[[{"type":"Text","text":"slow reply"}]]"#,
    );
    s.send_str("hi");
    s.send_key(&Key::Enter);
    // Type a draft while the run is in-flight (busy, not submitted).
    s.send_str(UNIQUE_TOKEN);
    // Esc aborts the run; the draft stays in the input box.
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        s.wait_for("Interrupted", RENDER_TIMEOUT),
        "Esc should abort the in-flight run:\n{}",
        s.output()
    );
    // Done(Interrupted) restores the run's origin into the input box ONLY
    // when the input is empty. A surviving draft (non-empty input) blocks
    // the restore, so "input restored" never lands — proving the draft is
    // still in the input box (a panic Esc did not destroy it). If Esc
    // wrongly cleared the draft, Done would restore the origin + surface
    // "input restored", failing this absence check.
    assert!(
        !s.wait_for_compact("inputrestored", RENDER_TIMEOUT),
        "the draft should survive the abort (no origin restore):\n{}",
        s.output()
    );
}

/// Ctrl+U clears a half-typed draft while a run is in-flight WITHOUT
/// aborting the run. Esc no longer clears the draft (it aborts), so Ctrl+U
/// is the one path to wipe a busy draft; this test pins that path so a
/// future change cannot silently remove the only clear-draft escape hatch.
#[test]
#[ignore]
fn test_ctrlu_clears_busy_draft() {
    let mut s = session_on_working_slow_in_repo(make_temp_repo(3), RUN_DELAY_MS);
    s.send_str("hi");
    s.send_key(&Key::Enter);
    s.send_str(UNIQUE_TOKEN);
    // Ctrl+U clears the draft; the run is not aborted.
    s.send_key(&Key::Ctrl('u'));
    std::thread::sleep(std::time::Duration::from_millis(200));
    s.clear_output();
    // A sentinel char proves the input box is alive + now holds only the new
    // char (the draft was wiped, not the run's input frozen).
    s.send_str("z");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !s.output_compact().contains(UNIQUE_TOKEN),
        "Ctrl+U should clear the draft:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains('z'),
        "input should still accept a new char after Ctrl+U:\n{}",
        s.output()
    );
    // Ctrl+U must not abort the run: the Interrupted notice never lands.
    // (If it did, the clear-draft escape hatch would double as a panic key,
    // defeating the Esc/ctrl-u split this test pins.)
    assert!(
        !s.wait_for_compact("Interrupted", RENDER_TIMEOUT),
        "Ctrl+U should not abort the run:\n{}",
        s.output()
    );
}

/// Esc while a run is in flight AND the queue holds a pending message: one
/// keystroke aborts the run AND pops the queue head into the input box for
/// editing. The popped text must NOT be auto-sent (the run ends Interrupted,
/// the clean-end auto-drain gate holds) and must NOT be clobbered by the
/// interrupt's input-restore path. Proven behaviorally: after the Esc, wait
/// for the interrupt notice (the abort really landed through the server),
/// then Enter. The queued token appearing as a contiguous user echo proves it
/// was sitting in the input box at submit time: an auto-send leaves the box
/// empty (no echo), and a clobber-by-restore submits the aborted run's origin
/// instead (the first message's text, not the token).
#[test]
#[ignore]
fn test_busy_esc_pops_queue() {
    let mut s = session_on_working_slow_in_repo(make_temp_repo(2), RUN_DELAY_MS);
    // Start a run (the stub's 3s delay keeps it in-flight with no content).
    s.send_str("first task");
    s.send_key(&Key::Enter);
    std::thread::sleep(std::time::Duration::from_millis(300));
    // Queue a message while busy (Enter routes to the pending queue).
    s.send_str(QUEUED_TOKEN);
    s.send_key(&Key::Enter);
    std::thread::sleep(std::time::Duration::from_millis(300));
    // Esc: abort + pop the queue head to the input box. The gap lets
    // crossterm resolve the bare 0x1b as Esc before anything follows.
    s.send_key(&Key::Esc);
    // The abort resolves through the server and lands the interrupt notice.
    assert!(
        s.wait_for("What should Houyi do instead", RENDER_TIMEOUT),
        "Esc should abort the in-flight run:\n{}",
        s.output()
    );
    // Wipe history so only what renders after the submit is read; the queued
    // token was typed char-by-char (never contiguous) and the popped input
    // box may render it, so the contiguous USER ECHO is the proof it was
    // submitted from the box - not auto-sent, not clobbered by the restore.
    s.clear_output();
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for(QUEUED_TOKEN, RENDER_TIMEOUT),
        "queued token should be popped to the input box and submit on Enter:\n{}",
        s.output()
    );
}

/// Shortcut: Ctrl+U kills to line start (readline semantics). Verified by
/// behavior, not input-box pixels (the box renders char-by-char so the typed
/// text is never a contiguous substring anyway): type a token, Ctrl+U, then
/// Enter. If Ctrl+U cleared the input, Enter submits an empty box (a no-op,
/// no user echo). If Ctrl+U failed, Enter submits the token and the user echo
/// renders the token as one contiguous line. So the token's contiguous presence
/// after Ctrl+U+Enter is the failure signal.
#[test]
#[ignore]
fn test_ctrl_u_clears_input() {
    let mut s = session_on_working();
    s.send_str(UNIQUE_TOKEN);
    // Wipe the char-by-char typed render so the absence check reads only what
    // renders after the Ctrl+U + Enter.
    s.clear_output();
    // Ctrl+U = 0x15 in a raw terminal.
    s.send_bytes(&[0x15]);
    s.send_key(&Key::Enter);
    assert!(
        !s.wait_for(UNIQUE_TOKEN, Duration::from_millis(600)),
        "Ctrl+U should clear the input so Enter submits nothing:\n{}",
        s.output()
    );
}

/// Shortcut: Esc in the idle working state with non-empty input clears the
/// input box (a concrete behavior instead of a dead key). Same behavioral
/// proof as ctrl_u_clears_input: clear, then Enter must submit nothing.
#[test]
#[ignore]
fn test_esc_clears_idle_input() {
    let mut s = session_on_working();
    s.send_str(UNIQUE_TOKEN);
    s.clear_output();
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    s.send_key(&Key::Enter);
    assert!(
        !s.wait_for(UNIQUE_TOKEN, Duration::from_millis(600)),
        "Esc should clear the idle input so Enter submits nothing:\n{}",
        s.output()
    );
}
