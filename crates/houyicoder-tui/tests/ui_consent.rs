//! Real-binary PTY UI tests for the git-consent checkpoint surface. #[ignore]
//! (each spawns the houyi binary + a PTY — too slow for the 60s commit gate).
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_consent -- --ignored after cargo build --bin houyi.
//!
//! The git-confirm checkpoint (git commit/rebase/reset/tag default Ask) is
//! exercised at the gate + render layers by the permission crate's unit
//! tests (the decide pipeline, the Ask verdict, the approval card render).
//! The interactive Ask CARD only fires on a real agent bash git-commit
//! call, which the stub provider (local-login mode) never makes — so the
//! card flow is not PTY-reachable here; it stays unit-covered.
//!
//! This binary covers the TOGGLE surface: /permissions git on|off drives
//! the wire round-trip that flips the gate flag + surfaces a system line
//! reflecting the new state. The palette now accepts the space separator,
//! so the arg-taking slash command is reachable through the real palette
//! path (the raw-submit branch fires when the spaced query matches no
//! palette entry).

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{RENDER_TIMEOUT, run_slash_command, session_on_working};

/// /permissions git on|off toggles the checkpoint. The wire round-trip sets
/// the gate flag; the system line reflects the new state. The toggle is
/// driven through the real palette path (space accepted), proving the
/// arg-taking command is reachable end-to-end.
#[test]
#[ignore]
fn test_git_toggle_round_trips() {
    let mut s = session_on_working();
    run_slash_command(&mut s, "permissions git off");
    assert!(
        s.wait_for("ask before git operations: off", RENDER_TIMEOUT),
        "git toggle off should surface:\n{}",
        s.output()
    );
    run_slash_command(&mut s, "permissions git on");
    assert!(
        s.wait_for("ask before git operations: on", RENDER_TIMEOUT),
        "git toggle on should surface:\n{}",
        s.output()
    );
}
