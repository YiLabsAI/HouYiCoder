//! UX-wiring PTY journey tests: real user journeys through the fixed surfaces
//! — /hooks opens a Pane (not a transcript dump), /model opens the model
//! selector, /resume <file> is reachable via the popup (hint-after-space),
//! /status shows the Status / Config / Usage sub-tabs, /debug is palette-
//! discoverable, and the resume picker disambiguates empty sessions. Drives the
//! real binary so each journey is exercised end-to-end, not just structure.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, run_slash_command};

/// /hooks opens the Hooks pane (a live view with a "Hooks" header), not a
/// transcript system-line dump. The user journey: type /hooks, see the pane.
#[test]
#[ignore]
fn test_hooks_opens_pane() {
    let mut s = PtySession::launch();
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "hooks");
    assert!(
        s.wait_for_plain("Hooks", RENDER_TIMEOUT),
        "/hooks should render the Hooks pane:\n{}",
        s.output_plain()
    );
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "Esc closes the hooks pane:\n{}",
        s.output_plain()
    );
}

/// /status shows the Status / Config / Usage sub-tabs (a Settings-modal-style
/// tabbed status). The user journey: type /status, see the tabs, Tab
/// cycles to Config.
#[test]
#[ignore]
fn test_status_renders_three_subtabs() {
    let mut s = PtySession::launch();
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "status");
    assert!(
        s.wait_for_plain("Status", RENDER_TIMEOUT),
        "/status should render the Status tab:\n{}",
        s.output_plain()
    );
    assert!(
        s.output_plain().contains("Config") && s.output_plain().contains("Usage"),
        "/status should render all three sub-tab titles:\n{}",
        s.output_plain()
    );
    // The active tab body renders real content, not just the three tab
    // titles above — a body field like "Model" (Status + Config tabs both
    // carry it) confirms the tab body drew. The prior test only pinned the
    // tab titles, leaving the body content unverified at the PTY layer.
    assert!(
        s.output_plain().contains("Model"),
        "/status tab body content missing (only tab titles were asserted before):\n{}",
        s.output_plain()
    );
}

/// The status bar context gauge renders on the working screen — the
/// always-visible observability surface (model · context% · mode). The
/// gauge label reads "X% context used" below 90%. Pins the gauge reaches
/// the real terminal byte stream, not just the unit-tested label fn.
#[test]
#[ignore]
fn test_status_bar_renders_gauge() {
    let mut s = PtySession::launch();
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    s.clear_output();
    // Let the status bar repaint a frame after clear.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        s.output().contains("context"),
        "status bar context gauge missing (context_window may be 0 in local mode):\n{}",
        s.output()
    );
}

/// The Usage sub-tab body renders the token breakdown labels (input / cache
/// read / cache write / tool calls). Tab cycles Status → Config → Usage; the
/// prior status test only pinned tab titles, so the Usage body content was
/// PTY-unverified.
#[test]
#[ignore]
fn test_status_usage_renders_tokens() {
    let mut s = PtySession::launch();
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "status");
    assert!(
        s.wait_for_plain("Status", RENDER_TIMEOUT),
        "/status should render the Status tab:\n{}",
        s.output_plain()
    );
    // Tab cycles: Status -> Config -> Usage.
    s.send_key(&Key::Tab);
    s.send_key(&Key::Tab);
    // Usage tab body carries the token labels (values are 0 on a fresh
    // session, but the labels render).
    let has_usage = s.wait_for("input", RENDER_TIMEOUT) || s.output().contains("cache read");
    assert!(
        has_usage,
        "Usage tab token labels missing (input / cache read):\n{}",
        s.output()
    );
}

/// /model opens the model selector Pane with the Default sentinel row + the
/// empty-state guide when no catalog is configured (the stub/local path).
#[test]
#[ignore]
fn test_model_opens_selector_pane() {
    let mut s = PtySession::launch();
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "model");
    assert!(
        s.wait_for_plain("Select a model", RENDER_TIMEOUT),
        "/model should render the model selector header:\n{}",
        s.output_plain()
    );
    let out = s.output_plain();
    assert!(
        out.contains("Default"),
        "/model selector should list the Default sentinel:\n{out}"
    );
    assert!(
        out.contains("no catalog configured"),
        "empty-state guide should render when no catalog:\n{out}"
    );
}

/// /resume <file> is reachable via the popup: the user types the command + a
/// space + the file path in the palette filter, and it resumes the export
/// (not silently running the argless picker). The hint-after-space guides the
/// argument. The user journey: /resume <fixture.json> → resumed session.
#[test]
#[ignore]
fn test_resume_file_reachable_popup() {
    let fixture = common::write_resume_fixture();
    let sessions_dir = common::fresh_temp_dir("sessions-resume-popup");
    let mut s = PtySession::launch_with_sessions_dir(None, None, None, None, &[], sessions_dir);
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    // Type the arg-bearing command in the palette: /, then "resume <path>".
    s.send_key(&Key::Char('/'));
    s.send_str(&format!("resume {}", fixture.to_string_lossy()));
    s.send_key(&Key::Enter);
    // In-process swap with skip_login: lands on the working screen directly
    // (no re-login). The fixture's model name in the status bar is the signal
    // the swap took effect — clear the scrollback first so the wait does not
    // match the pre-swap frame.
    s.clear_output();
    assert!(
        s.wait_for("stub-resume-model", RENDER_TIMEOUT),
        "export resume should swap to the working screen with the fixture model:\n{}",
        s.output_plain()
    );
}

/// /debug is palette-discoverable and executes: the registered command runs
/// via the palette and reports the log path. The user journey: /, type debug,
/// run it. (Palette discovery is unit-tested in the protocol crate; this PTY
/// verifies the dispatched run produces the expected output.)
#[test]
#[ignore]
fn test_debug_palette_visible_runs() {
    let mut s = PtySession::launch();
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "debug");
    assert!(
        s.wait_for_plain("debug: logging to", RENDER_TIMEOUT),
        "/debug should run and report the log path:\n{}",
        s.output_plain()
    );
}

/// The resume picker disambiguates empty sessions: several sessions with no
/// name + no prompt render distinguishable titles (a short sid suffix), not
/// all "(session)". The user journey: /resume in a dir with two empty
/// sessions → two distinct rows.
#[test]
#[ignore]
fn test_resume_picker_disambiguates_empty() {
    let sessions_dir = fresh_temp_dir("sessions-picker-disambig");
    let sid_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let sid_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    common::seed_meta_only(&sessions_dir, sid_a);
    common::seed_meta_only(&sessions_dir, sid_b);
    let mut s = PtySession::launch_with_sessions_dir(None, None, None, None, &[], sessions_dir);
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_plain("Resume a session", RENDER_TIMEOUT),
        "resume picker should open:\n{}",
        s.output_plain()
    );
    // Wait for the second session to render before reading the buffer —
    // the picker lists sessions asynchronously and under load (e.g. parallel
    // reward_smoke in make verify) the second row may lag the first.
    assert!(
        s.wait_for_plain("bbbbbbbb", RENDER_TIMEOUT),
        "second session should appear in picker:\n{}",
        s.output_plain()
    );
    let out = s.output_plain();
    // Two distinct placeholders (each carries a different short sid suffix),
    // not a single undifferentiated "(session)".
    assert!(
        out.matches("(session)").count() >= 2,
        "picker should list both empty sessions:\n{out}"
    );
    assert!(
        out.contains("aaaaaaaa") && out.contains("bbbbbbbb"),
        "picker rows should carry the disambiguating short sid:\n{out}"
    );
}
