//! Real-binary PTY tests for the /context command surface. The unit layer
//! (context_view.rs inline tests + context_command_tests.rs) asserts on App
//! state + wire struct fields; this layer drives the real houyi binary through
//! a real terminal so the rendered byte stream the user actually sees is
//! pinned: every section is present, the legend carries the model/window/pct
//! line + category rows, zero turns still give honest prospective data (the
//! "no data yet" empty-grid guard does not fire on a real wired session
//! because app.session is always Some), and a post-compact session shows the
//! folded summary + Compact buffer category + Cache prefix line.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_context -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{PtySession, RENDER_TIMEOUT, fresh_temp_dir, run_slash_command, session_on_working};

/// /context renders every section on a fresh zero-turn session. The session is always Some in the real binary (composition wires
/// app.session = Some(live_session)), so /context hits the server's real
/// prospective breakdown — not the canned stub the no-session path would
/// render. That stub path (100% full fake data) is unreachable in production
/// and is not asserted here. session_on_working picks local mode (sends
/// '3' on the login screen) to land on the working screen without a network
/// call.
#[test]
#[ignore]
fn test_context_renders_sections() {
    let mut s = session_on_working();
    s.clear_output();
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for("Context Usage", RENDER_TIMEOUT),
        "bold header missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("Estimated usage by category"),
        "legend header missing:\n{}",
        s.output()
    );
    // A real category row (Free space renders when free tokens > 0, which
    // holds for a fresh session with a large window + tiny system prompt).
    // Asserting a real label distinguishes the category row from the
    // model/window/pct legend line, which "tokens (" alone cannot.
    assert!(
        s.output().contains("Free space:"),
        "free-space category row missing:\n{}",
        s.output()
    );
    // Cache prefix renders unconditionally (dispatch always fills it from
    // System prompt + Tools tokens). At zero turns hit_rate is None (no
    // prior provider turn) — this asserts the decoupled prefix line still
    // shows, the exact case the wire-render bug hid. Pins the fix in the
    // happy path, not just the post-compact path.
    assert!(
        s.output().contains("Cache prefix:"),
        "cache prefix line missing on a zero-turn session:\n{}",
        s.output()
    );
    // The empty-grid guard ("no data yet") must NOT fire on a real wired
    // session even with zero turns — the prospective breakdown is non-empty.
    assert!(
        !s.output().contains("no data yet"),
        "empty-grid guard fired on a real session:\n{}",
        s.output()
    );
}

/// /context after a compaction shows the folded summary line + the
/// Compact buffer category + the Cache prefix line. Seeds a session with a
/// checkpoint manifest on disk (a post-compact state) via the real
/// LocalFileBackend, launches --resume <sid>, and pins that the /context
/// render surfaces the post-compact state — the interaction the unit layer
/// could not reach because it never drove compact-then-/context through the
/// real dispatch + render path.
#[test]
#[ignore]
fn test_post_compact_shows_folded() {
    let sessions_dir = fresh_temp_dir("sessions-ctx-compact");
    let sid = "44444444-4444-4444-4444-444444444444";
    common::seed_session_with_checkpoint(
        &sessions_dir,
        sid,
        "ctx-compact-model",
        "seeded prompt",
        "summary of folded turns",
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir,
    );
    if !s.wait_for("let's build, or / for commands", RENDER_TIMEOUT) {
        panic!("working screen after resume:\n{}", s.output());
    }
    s.clear_output();
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for("Context Usage", RENDER_TIMEOUT),
        "bold header missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("Compact buffer"),
        "compact buffer category missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("folded:"),
        "folded summary line missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("Cache prefix:"),
        "cache prefix line missing:\n{}",
        s.output()
    );
    // The cache breakpoint bar (U+2502) renders on the grid at the cell where
    // the cached prefix ends — the real-binary verification of the breakpoint
    // marker.
    assert!(
        s.output().contains('\u{2502}'),
        "cache breakpoint bar missing on the grid:\n{}",
        s.output()
    );
}
