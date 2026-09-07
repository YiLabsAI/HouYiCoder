//! End-to-end terminal tests for interruption and queued input.
//!
//! A delayed stub provides a deterministic window before assistant output so
//! immediate cancellation exercises input restoration without a timing race.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, pty_session_slow};

/// Large enough that the stub's first delta lands well after the test's key
/// sequence. The run is in-flight (agent_busy, spinner live) for this whole
/// window with zero streamed content, so an Esc right after Enter is always a
/// pre-content abort.
const RUN_DELAY_MS: u64 = 3000;

/// Esc on an in-flight run that has streamed no real content aborts it,
/// rewinds the user echo + any partial, and restores the input so the user can
/// edit and resend. The unit layer cannot reach this — it needs the real run
/// chain + the transcript rebuild on the Interrupted outcome. The large stub
/// delay guarantees no delta lands before the cancel, so the restore path fires
/// and renders the "input restored" system line.
#[test]
#[ignore]
fn test_esc_aborts_restores_input() {
    let mut s = pty_session_slow(RUN_DELAY_MS);
    s.send_str("hi");
    s.send_key(&Key::Enter);
    // Esc immediately: the run is in-flight, the submit cleared the input, so
    // the busy+empty branch aborts. Lands before the first delta (RUN_DELAY_MS
    // away), so the run produced no real content -> rewind + restore.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("input restored", RENDER_TIMEOUT),
        "Esc before any content should abort + restore the input:\n{}",
        s.output()
    );
    // The stub's canned reply never streamed (cancelled before the first delta)
    // and the rewind dropped the user echo, so the reply marker is absent.
    assert!(
        !s.output().contains("stub mode: no api key"),
        "the stub reply should not render after a pre-content abort:\n{}",
        s.output()
    );
}

/// A second Enter while a run is in-flight queues the input and the
/// ambient queue strip renders it above the input box. The unit layer
/// asserts the strip's content; this pins the real repaint path (the
/// strip appears through the working-surface render, not a TestBackend
/// dump).
#[test]
#[ignore]
fn test_queue_strip_while_busy() {
    let mut s = pty_session_slow(RUN_DELAY_MS);
    s.send_str("first");
    s.send_key(&Key::Enter);
    // The run is in-flight; a second submit queues instead of spawning.
    s.send_str("second");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("→ next", RENDER_TIMEOUT),
        "a non-empty queue should render the strip head:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("second"),
        "the queued message should appear in the strip:\n{}",
        s.output()
    );
}
