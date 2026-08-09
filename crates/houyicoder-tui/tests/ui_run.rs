//! Real-binary PTY tests for the run-lifecycle interactions: Esc abort +
//! rewind/restore (#12), and the Ctrl+G queue-manager hint (#13). The unit
//! layer covers the state-machine decisions; this layer drives the real houyi
//! binary through a real terminal so the run chain (driver -> wire -> server ->
//! runner -> cancel token) + the transcript rebuild on Interrupted are pinned
//! end-to-end. These are the bugs the unit layer let through because the
//! breakage only surfaces when the real crossterm loop + the wire + the
//! repaint all chain together.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_run -- --ignored after cargo build --bin houyi.
//!
//! The stub streams with a large inter-chunk delay (HOUYICODER_STUB_DELAY_MS)
//! so the run stays in-flight with NO streamed content for a clean window:
//! the stub's first delta lands RUN_DELAY_MS after Enter, so an Esc sent right
//! after Enter always lands before any content — run_produced_real_content is
//! false and the rewind/restore path fires deterministically (no race).

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_slow};

/// Large enough that the stub's first delta lands well after the test's key
/// sequence. The run is in-flight (agent_busy, spinner live) for this whole
/// window with zero streamed content, so an Esc right after Enter is always a
/// pre-content abort.
const RUN_DELAY_MS: u64 = 3000;

/// #12: Esc on an in-flight run that has streamed no real content aborts it,
/// rewinds the user echo + any partial, and restores the input so the user can
/// edit and resend. The unit layer cannot reach this — it needs the real run
/// chain + the transcript rebuild on the Interrupted outcome. The large stub
/// delay guarantees no delta lands before the cancel, so the restore path fires
/// and renders the "input restored" system line.
#[test]
#[ignore]
fn test_esc_aborts_restores_input() {
    let mut s = session_on_working_slow(RUN_DELAY_MS);
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
        !s.output().contains("stub mode: no api key set"),
        "the stub reply should not render after a pre-content abort:\n{}",
        s.output()
    );
}

/// #13: a second Enter while a run is in-flight queues the input and the
/// ambient queue strip renders the Ctrl+G manager hint. The unit layer asserts
/// the strip's content; this pins the real repaint path (the strip appears
/// through the working-surface render, not a TestBackend dump).
#[test]
#[ignore]
fn test_ctrl_g_hint_queued() {
    let mut s = session_on_working_slow(RUN_DELAY_MS);
    s.send_str("first");
    s.send_key(&Key::Enter);
    // The run is in-flight; a second submit queues instead of spawning.
    s.send_str("second");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("Ctrl+G to manage", RENDER_TIMEOUT),
        "a non-empty queue should show the Ctrl+G hint:\n{}",
        s.output()
    );
}
