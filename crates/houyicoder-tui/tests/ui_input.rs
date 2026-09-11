//! Real-binary PTY tests for input-box key behavior: the busy-Esc gate and
//! the readline-style Ctrl+U clear shortcut. The unit layer covers InputBuffer
//! mutation and the keys.rs gate; this
//! layer drives the real crossterm byte path so the key-routing + repaint chain
//! is pinned, not just the state machine.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_input -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{
    Key, RENDER_TIMEOUT, pty_session, pty_session_slow_in_repo, pty_session_slow_scripted,
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
    let mut s =
        pty_session_slow_scripted(RUN_DELAY_MS, r#"[[{"type":"Text","text":"slow reply"}]]"#);
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
    let mut s = pty_session_slow_in_repo(make_temp_repo(3), RUN_DELAY_MS);
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

/// Repeated Esc while a run and queued message are active remains an abort.
/// Once interruption settles, the queue renders as held instead of moving its
/// text into the input box through a timing-dependent second action.
#[test]
#[ignore]
fn test_esc_keeps_queue() {
    let mut s = pty_session_slow_in_repo(make_temp_repo(2), RUN_DELAY_MS);
    s.send_str("first task");
    s.send_key(&Key::Enter);
    s.send_str(QUEUED_TOKEN);
    s.send_key(&Key::Enter);
    assert!(s.wait_for("→ next", RENDER_TIMEOUT));

    s.send_key(&Key::Esc);
    s.send_key(&Key::Esc);

    assert!(
        s.wait_for("What should Houyi do instead", RENDER_TIMEOUT),
        "Esc should abort the in-flight run:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("⏸", RENDER_TIMEOUT),
        "repeated Esc must leave the queue held:\n{}",
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
    let mut s = pty_session();
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

/// Streaming CJK text leaves no isolated user-background cells on the final
/// terminal screen.
#[test]
#[ignore]
fn test_cjk_background_clean() {
    let marker = "\u{80cc}\u{666f}\u{68c0}\u{67e5}\u{5b8c}\u{6210}";
    let cjk = "\u{4e2d}\u{6587}\u{5bbd}\u{5b57}\u{7b26}\u{6d41}\u{5f0f}\u{5237}\u{65b0}";
    let response = format!("{}{marker}", cjk.repeat(300));
    let script = serde_json::json!([[{"type":"Text", "text": response}]]).to_string();
    let mut s = pty_session_slow_scripted(1, &script);
    s.send_str("\u{8bf7}\u{8fde}\u{7eed}\u{8f93}\u{51fa}\u{5bbd}\u{5b57}\u{7b26}");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen(marker, RENDER_TIMEOUT * 2),
        "scripted CJK response should finish:\n{}",
        s.output()
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    let screen = s.screen();
    let (rows, cols) = screen.size();
    let residual: Vec<(u16, u16)> = (0..rows)
        .flat_map(|row| (0..cols).map(move |col| (row, col)))
        .filter(|(row, col)| {
            screen.cell(*row, *col).is_some_and(|cell| {
                !cell.is_wide_continuation() && cell.bgcolor() == vt100::Color::Idx(238)
            })
        })
        .collect();
    assert!(
        residual.is_empty(),
        "gray cells remained after the user row scrolled away: {residual:?}\n{}",
        screen.contents()
    );
}

/// The native cursor remains hidden while the painted caret is visible.
#[test]
#[ignore]
fn test_native_cursor_hidden() {
    let mut s = pty_session();
    s.clear_output();
    s.send_str("x");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !s.output().contains("\u{1b}[?25h"),
        "redraw must not show the hardware cursor:\n{}",
        s.output()
    );
}

/// Idle Esc leaves editor content intact. Ctrl+U is the explicit clear action,
/// so a fast second Esc cannot erase text restored by an interruption.
#[test]
#[ignore]
fn test_esc_keeps_input() {
    let mut s = pty_session();
    s.send_str(UNIQUE_TOKEN);
    s.clear_output();
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for(UNIQUE_TOKEN, RENDER_TIMEOUT),
        "Esc should preserve idle input for submission:\n{}",
        s.output()
    );
}
