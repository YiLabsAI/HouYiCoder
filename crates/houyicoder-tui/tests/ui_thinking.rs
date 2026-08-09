//! Real-binary PTY test for the thinking indicator during a streamed turn.
//! A scripted response carries a Reasoning item + a guarded bash ToolCall; in
//! Manual mode the bash ASKs, and raise_agent_approval does NOT clear
//! live_active/live_reasoning_text (only Done does). The live ∴ Thinking
//! block was removed (live reasoning does not echo
//! as a block); the thinking indicator is now the spinner row's
//! "Thinking" verb. This pins that the block stays gone end-to-end.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_thinking -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_with_script};

/// A response that streams a reasoning item, then a bash ToolCall (the bash
/// ASKs in Manual mode), then plain text to end the run after the approve.
const REASONING_THEN_BASH_SCRIPT: &str = r#"[
  [{"type":"Reasoning","text":"analyzing the request carefully step by step"},
   {"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo hi"}}],
  [{"type":"Text","text":"done"}]
]"#;

/// No live ∴ Thinking block renders during a reasoning turn, through the real
/// binary. In Manual mode the bash ToolCall raises an approval card; the run
/// pauses on it with live_active still true (raise_agent_approval does not
/// clear it). The thinking indicator is the spinner row's "Thinking" verb —
/// the ∴ block must not surface a ctrl+o hint on every interaction.
#[test]
#[ignore]
fn test_no_live_thinking_block() {
    let mut s = session_on_working_with_script(REASONING_THEN_BASH_SCRIPT);
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
    // The reasoning streams into live_reasoning_text, then the bash ToolCall
    // raises the approval card — the run pauses, live_active stays true. The
    // spinner carries the "Thinking" verb; the ∴ Thinking block must NOT
    // render (live reasoning does not echo as a block).
    s.wait_for("Thinking", RENDER_TIMEOUT);
    assert!(
        !s.output().contains("∴ Thinking"),
        "the live ∴ Thinking block must not render during the turn:\n{}",
        s.output()
    );
    // Approve the bash (default Yes focus) so the run resumes + ends cleanly.
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "approving should resume the run + render done:\n{}",
        s.output()
    );
}
