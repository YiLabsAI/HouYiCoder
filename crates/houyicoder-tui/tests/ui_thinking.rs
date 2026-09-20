//! Real-binary PTY test for the live thinking block. The script carries a
//! Reasoning item then a guarded bash ToolCall; in Manual mode the bash ASKs
//! and the run pauses with live state still set. Live reasoning does not echo
//! as a block, so the block must stay gone while paused.
//!
//! Run via make suite ui (builds the bin first) or
//! cargo test --test ui_all ui_thinking:: -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

use crate::common::{Key, RENDER_TIMEOUT, pty_session_scripted};

/// A response that streams a reasoning item, then a bash ToolCall (the bash
/// ASKs in Manual mode), then plain text to end the run after the approve.
const REASONING_THEN_BASH_SCRIPT: &str = r#"[
  [{"type":"Reasoning","text":"analyzing the request carefully step by step"},
   {"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo hi"}}],
  [{"type":"Text","text":"done"}]
]"#;

/// Live reasoning does not echo as a block during a reasoning turn, through
/// the real binary. In Manual mode the bash ToolCall raises an approval card;
/// the run pauses on it with live_active still true, so the paused render is
/// where a live reasoning echo would surface.
#[test]
#[ignore]
fn test_no_live_thinking_block() {
    let mut s = pty_session_scripted(REASONING_THEN_BASH_SCRIPT);
    // Auto mode auto-approves (no card, no pause); cycle to Manual so the bash
    // ASKs + the run pauses with the live state visible.
    s.send_key(&Key::Backtab);
    assert!(
        s.wait_for("manual mode on", RENDER_TIMEOUT),
        "shift+tab should cycle to manual mode:\n{}",
        s.output()
    );
    s.send_str("go");
    s.send_key(&Key::Enter);
    // The approval card is the latch: it renders only once the reasoning has
    // streamed and the paused ToolCall is waiting on the verdict.
    assert!(
        s.wait_for("1. Yes", RENDER_TIMEOUT),
        "the guarded bash should raise the approval card:\n{}",
        s.output()
    );
    // The streamed reasoning text is the latch. A live block would echo it
    // verbatim; the collapsed transcript line carries the duration only, so
    // the text appearing here would mean the live echo came back.
    assert!(
        !s.output_compact()
            .contains("analyzingtherequestcarefullystepbystep"),
        "live reasoning must not echo as a block during the turn:\n{}",
        s.output()
    );
    // Approve the bash (default Yes focus) so the run resumes + ends cleanly.
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "approving should resume the run + render done:\n{}",
        s.output()
    );
    // The reasoning reached the transcript as a collapsed line, so the
    // absence above is a real fold rather than an item that never streamed.
    assert!(
        s.output_compact().contains("✻Thoughtfor"),
        "the reasoning should land as a collapsed transcript line:\n{}",
        s.output()
    );
}
