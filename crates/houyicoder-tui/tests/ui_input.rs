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

use common::{Key, RENDER_TIMEOUT, session_on_working, session_on_working_slow_in_repo};
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

/// #15: Esc while a run is in-flight AND the input has a draft must clear the
/// draft (not abort the run), so a second Esc is needed to abort. The gate is
/// the fix; without it the first Esc would abort, stranding any way to wipe a
/// recalled draft while busy. Proven without a flaky busy-marker by clearing
/// output between the two Escs: if the first Esc wrongly aborted, the
/// "input restored" line it produced is wiped by clear_output and the second
/// Esc (now idle) is a no-op, so wait_for(input restored) fails. When the gate
/// works, the first Esc clears the draft (run stays alive), the second Esc
/// (busy + empty) aborts + restores, and "input restored" renders fresh.
#[test]
#[ignore]
fn test_esc_clears_keeps_run() {
    let mut s = session_on_working_slow_in_repo(make_temp_repo(1), RUN_DELAY_MS);
    s.send_str("hi");
    s.send_key(&Key::Enter);
    // Type a draft while the run is in-flight. It is not submitted (busy), so
    // it sits in the input box.
    s.send_str(UNIQUE_TOKEN);
    // First Esc: busy + non-empty input -> clears the draft, does NOT abort.
    // Sleep after the byte: crossterm reads a bare 0x1b as Esc only when no
    // following byte arrives within its escape-sequence window, so a gap is
    // needed before the next send (the second Esc) or the two merge.
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Wipe history so a "input restored" from a (wrong) first-Esc abort is
    // gone; the second Esc must produce it fresh to pass.
    s.clear_output();
    // Second Esc: the draft is cleared so input is empty -> busy+empty aborts.
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        s.wait_for("input restored", RENDER_TIMEOUT),
        "first Esc should clear the draft (not abort); second Esc should abort + restore:\n{}",
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
