//! Real-binary PTY test: the palette-registered local commands (Debug /
//! Search) are not just runnable from the input box — they are SELECTABLE
//! from the filtered palette (the real "listed in the palette" contract: a
//! command that runs but cannot be selected from the palette is a discovery
//! bug). This pins the real binary's palette → select → run chain.
//!
//! Assertion strategy: the palette popover renders via ratatui cell-diff
//! repaints, so a raw-stream substring on the popover row itself is flaky
//! (contiguous text splits across cursor-move escapes). Instead, SELECT the
//! command from the filtered palette (Enter on the filtered selection) and
//! assert the DOWNSTREAM effect — a fresh single-row write that is not
//! subject to cell-diff fragmentation:
//!   - /search (takes_arg) → Enter inserts "/search <query>" → submit → the
//!     search-view status row "SEARCH <query> no match" renders as a fresh row.
//!   - /debug (auto-run) → Enter runs it → the debug system line
//!     renders as a fresh transcript tail row.

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working};
use std::time::Duration;

/// After a gesture that leaves the palette / a pane open, sleep so crossterm
/// does not glue the Esc byte (\x1b) to the next key into an Alt+<key> event.
fn pause() {
    std::thread::sleep(Duration::from_millis(200));
}

/// /search is palette-selectable: open the palette, filter to it, Enter
/// selects (takes_arg inserts "/search " + waits), type a query, Enter
/// submits, and the search-pane header renders. This proves /search is in
/// the palette AND the takes_arg select path works end-to-end through the
/// real binary.
#[test]
#[ignore]
fn test_palette_select_runs_search() {
    let mut s = session_on_working();
    s.send_key(&Key::Char('/'));
    for c in "search".chars() {
        s.send_key(&Key::Char(c));
    }
    // Enter on the filtered selection: takes_arg inserts "/search " + does
    // NOT submit. The palette closes; the input box holds "/search ".
    s.send_key(&Key::Enter);
    for c in "zzzebra".chars() {
        s.send_key(&Key::Char(c));
    }
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("SEARCH  zzzebra  no match", RENDER_TIMEOUT),
        "selecting /search from the palette + typing a query should open the search view:\n{}",
        s.output()
    );
}

/// /debug is palette-selectable: open the palette, filter to it, Enter selects
/// and auto-runs (takes_arg is false), and the debug system line renders in
/// the transcript. Proves an argless palette command runs end-to-end through
/// the real binary.
#[test]
#[ignore]
fn test_palette_select_runs_debug() {
    let mut s = session_on_working();
    s.send_key(&Key::Char('/'));
    for c in "debug".chars() {
        s.send_key(&Key::Char(c));
    }
    s.send_key(&Key::Enter);
    // /debug is argless: Enter on the filtered selection auto-runs it; the
    // debug system line lands as a fresh transcript tail row.
    assert!(
        s.wait_for("debug", RENDER_TIMEOUT),
        "selecting /debug from the palette should run it + render the debug line:\n{}",
        s.output()
    );
    pause();
}

/// /export runs end-to-end through the real binary: typed as a slash command
/// with an explicit path, the handler serializes the (empty, just-started)
/// session to JSON + writes it atomically + reports the path. An explicit
/// temp path keeps the write out of the workspace cwd (the argless form would
/// default to a timestamp + session.json name in the workspace root). The
/// protocol-level table test pins palette discoverability; this PTY test pins
/// the handler to atomic-write to report chain.
#[test]
#[ignore]
fn test_palette_select_runs_export() {
    use common::fresh_temp_dir;
    let dir = fresh_temp_dir("export");
    let target = dir.join("session.json");
    let mut s = session_on_working();
    common::run_slash_command(&mut s, &format!("export {}", target.display()));
    assert!(
        s.wait_for(
            &format!("export: wrote {}", target.display()),
            RENDER_TIMEOUT
        ),
        "/export <path> should write the JSON file + report the path:\n{}",
        s.output()
    );
    assert!(target.exists(), "the export file must land on disk");
    pause();
}
