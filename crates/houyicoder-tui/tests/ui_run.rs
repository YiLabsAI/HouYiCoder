//! End-to-end terminal tests for interruption and queued input.
//!
//! A delayed stub provides a deterministic window before assistant output so
//! immediate cancellation exercises input restoration without a timing race.

#![allow(clippy::unwrap_in_result)]

use houyicoder_tui::records::INTERRUPTED_NOTICE;

use crate::common::{Key, RENDER_TIMEOUT, pty_session_slow, pty_session_slow_scripted};

/// Large enough that the stub's first delta lands well after the test's key
/// sequence. The run is in-flight (agent_busy, spinner live) for this whole
/// window with zero streamed content, so an Esc right after Enter is always a
/// pre-content abort.
const RUN_DELAY_MS: u64 = 3000;

/// The stub delay for a queued journey: enough for a submit sent right after
/// Enter to land while the run is still active, without holding the journey
/// open for the full interrupt delay.
const QUEUED_RUN_DELAY_MS: u64 = 400;

/// The queued message's text, distinctive enough that finding it on the
/// settled screen means the drain carried this text into the next turn.
const QUEUED_TOKEN: &str = "zzqueued";

/// Esc on an in-flight run that has streamed no real content aborts it,
/// rewinds the user echo + any partial, and restores the input so the user can
/// edit and resend. The unit layer cannot reach this — it needs the real run
/// chain + the transcript rebuild on the Interrupted outcome. The large stub
/// delay guarantees no delta lands before the cancel, so the restore path fires
/// and renders the "input restored" system line.
#[test]
#[ignore]
fn test_esc_restores_input() {
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
    s.clear_output();
    s.send_key(&Key::Esc);
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("hi", RENDER_TIMEOUT),
        "a repeated Esc must not erase the restored input:\n{}",
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

/// A message queued while a run is active drains on its own when that run
/// finishes: no keypress promotes it and the queued text becomes the next
/// turn. The journey also renders no interruption notice — the user stopped
/// nothing. The notice is a render-layer row with its own lifecycle, so the
/// absence check reads the whole byte stream rather than the settled screen:
/// a row that flashes and vanishes is what a screen-only check misses.
#[test]
#[ignore]
fn test_queue_drains_without_keys() {
    // One scripted turn per run, so each run's reply is its own marker and
    // the drain is visible as the second reply arriving with no key sent.
    let script = r#"[[{"type":"Text","text":"first reply done"}],
                     [{"type":"Text","text":"second reply done"}]]"#;
    let mut s = pty_session_slow_scripted(QUEUED_RUN_DELAY_MS, script);
    s.send_str("first");
    s.send_key(&Key::Enter);
    s.send_str(QUEUED_TOKEN);
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("→ next", RENDER_TIMEOUT),
        "the active run queues the second message:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_screen("second reply done", RENDER_TIMEOUT * 3),
        "the queued message drains into its own run once the first finishes, \
         with no key sent:\n{}",
        s.screen().contents()
    );
    assert!(
        !s.screen().contents().contains("→ next"),
        "the drained queue leaves no strip behind:\n{}",
        s.screen().contents()
    );
    // The strip is gone and the input box cleared at submit, so the token can
    // only be the transcript echo of the drained message: the drain carried
    // the text into the next turn rather than starting a run without it. The
    // count is pinned, not just presence: a stale queue row rendered under
    // some other head label would satisfy a presence check through the strip.
    let screen = s.screen().contents();
    assert_eq!(
        screen.matches(QUEUED_TOKEN).count(),
        1,
        "the queued text became the second turn's input, once:\n{screen}"
    );
    // The notice's own wording, taken past the glyph and indent the renderer
    // emits as separate cells, so a reworded notice cannot leave this absence
    // check silently true.
    let from = INTERRUPTED_NOTICE
        .find("Interrupted")
        .expect("the notice names the interruption");
    assert!(
        !s.output_plain().contains(&INTERRUPTED_NOTICE[from..]),
        "a journey where the user stopped nothing renders no interruption \
         notice:\n{}",
        s.output_plain()
    );
}
