//! Real-binary PTY UI tests for the permission MODE switching. #[ignore]
//! (each spawns the houyi binary + a PTY — too slow for the 60s commit gate).
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_mode -- --ignored after cargo build --bin houyi.
//!
//! Mode switching is Shift+Tab only — the canonical mechanism. There is no
//! /mode or /mode-log slash command (those were self-invention; the switch
//! audit belongs to the trajectory
//! observability surface, recorded there later). The status-bar pill shows
//! the current mode. These tests cover the pill render + the real carrier
//! round-trip for the cycle, INCLUDING the dynamic mid-run switch (the
//! mid-run switch lets you cycle mode while a run is
//! in flight; a gate that blocked BackTab while agent_busy would regress it).

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working, session_on_working_slow};

/// At session start the status pill renders the default mode (Auto). Proves
/// the status bar + the mode pill render through the real repaint path.
#[test]
#[ignore]
fn test_default_mode_pill_renders() {
    let mut s = session_on_working();
    assert!(
        s.wait_for("auto mode on", RENDER_TIMEOUT),
        "default auto pill should render:\n{}",
        s.output()
    );
}

/// Shift+Tab cycles the mode through the REAL carrier round-trip: tab_cycle
/// ships a PermissionCycleModeQuery (no optimistic local update), the server
/// cycles + replies with PermissionMode, the response lands in mode_cache,
/// and the pill re-renders. A pill flip here proves the wire round-trip
/// actually updates TUI state — the stronger carrier test for mode switching,
/// which the unit layer cannot reach.
#[test]
#[ignore]
fn test_shift_tab_cycles_mode() {
    let mut s = session_on_working();
    assert!(
        s.wait_for("auto mode on", RENDER_TIMEOUT),
        "default auto pill should render:\n{}",
        s.output()
    );
    s.send_key(&Key::Backtab);
    assert!(
        s.wait_for("manual mode on", RENDER_TIMEOUT),
        "shift+tab should cycle auto -> manual:\n{}",
        s.output()
    );
    s.send_key(&Key::Backtab);
    assert!(
        s.wait_for("auto mode on", RENDER_TIMEOUT),
        "shift+tab should cycle manual -> auto:\n{}",
        s.output()
    );
}

/// DYNAMIC mode switch: Shift+Tab cycles the mode WHILE a run is in flight
/// (agent_busy). Mid-run cycling is allowed; a gate on
/// agent_busy would block BackTab + the pill would not flip. The stub
/// streams with an inter-chunk delay (HOUYI_STUB_DELAY_MS) so the run stays
/// in-flight long enough for the mid-run Shift+Tab to land. If the cycle is
/// blocked while busy, the pill stays on auto and this test fails — the
/// regression guard for the dynamic-switch alignment.
#[test]
#[ignore]
fn test_shift_tab_cycles_run() {
    let mut s = session_on_working_slow(50);
    assert!(
        s.wait_for("auto mode on", RENDER_TIMEOUT),
        "default auto pill should render:\n{}",
        s.output()
    );
    // Start a run (agent_busy flips true synchronously on Enter). The stub's
    // delayed stream keeps the run in-flight for the mid-run key.
    s.send_str("hi");
    s.send_key(&Key::Enter);
    // Immediately cycle the mode mid-run. No sleep: the Shift+Tab byte is
    // queued in stdin right after Enter, and the run's Done has to traverse
    // the driver -> wire -> server -> runner -> provider chain, so the key
    // is read while agent_busy is still true.
    s.send_key(&Key::Backtab);
    assert!(
        s.wait_for("manual mode on", RENDER_TIMEOUT),
        "shift+tab should cycle the mode while a run is in flight:\n{}",
        s.output()
    );
}
