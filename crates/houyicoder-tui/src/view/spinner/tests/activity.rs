//! Working and Thinking indicator presentation tests.

#![cfg(test)]

use crate::agent_message::{ServerEvent, SessionMessage};
use crate::composition;
use crate::state::App;
use crate::state::Screen;
use crate::state::enums::LiveBlock;
use crate::test_harness::render_buffer;
use crate::test_harness::render_text;
use ratatui::style::Color;

fn working() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app
}

#[test]
fn test_spinner_verb_reflects_phase() {
    // The spinner verb tracks the active stream phase.
    let mut app = working();
    app.start_run_for_test(0);
    // Reasoning streaming ⇒ Thinking.
    {
        let p = app.run_progress_mut().expect("active run");
        p.live_reasoning_text = "pondering the task".into();
        p.live_block = LiveBlock::Thinking;
    }
    let out = render_text(&app, 80, 12);
    assert!(
        out.contains("Thinking"),
        "reasoning phase should show Thinking:\n{out}"
    );
    // No reasoning streaming ⇒ Working.
    {
        let p = app.run_progress_mut().expect("active run");
        p.live_reasoning_text.clear();
        p.live_block = LiveBlock::None;
    }
    let out = render_text(&app, 80, 12);
    assert!(
        out.contains("Working"),
        "non-reasoning phase should show Working:\n{out}"
    );
    assert!(
        !out.contains("Optimizing"),
        "stale random verb must be gone:\n{out}"
    );
}

/// Assistant output shows Working while retained reasoning remains available
/// for the post-turn summary.
#[test]
fn test_verb_works_text_streams() {
    let mut app = working();
    app.start_run_for_test(0);
    // Reasoning streamed first, then assistant text takes over.
    {
        let p = app.run_progress_mut().expect("active run");
        p.live_reasoning_text = "pondered".into();
        p.live_assistant_text = "Here is the answer".into();
        p.live_block = LiveBlock::Responding;
    }
    let out = render_text(&app, 80, 12);
    assert!(
        out.contains("Working"),
        "text streaming should show Working, not Thinking:\n{out}"
    );
    assert!(
        !out.contains("Thinking"),
        "stale Thinking must not persist once text streams:\n{out}"
    );
}

/// The token estimate counts the whole stream, not the bounded display form:
/// a reply far past the preview's bound still drives the meter to what the
/// stream holds, where a count over the display form alone would stop at the
/// bound.
#[test]
fn test_token_estimate_counts_stream() {
    let mut app = working();
    app.start_run_for_test(0);
    for _ in 0..40 {
        app.handle_agent_message(SessionMessage::Event(ServerEvent::Delta {
            text: "x".repeat(500),
        }));
    }
    // The meter lerps toward the estimate, so a few draws settle it.
    let mut out = String::new();
    for _ in 0..40 {
        out = render_text(&app, 200, 12);
    }
    assert!(
        out.contains("↓ 5.0k tokens"),
        "20 KB of stream reads as 5k tokens, not the display form's:\n{out}"
    );
}

#[test]
fn test_busy_border_stays_plain() {
    // Terminal progress owns the processing signal; the pane border stays plain.
    let is_shimmer = |c: Color| matches!(c, Color::Rgb(0, g, b) if g == b && g > 0);
    for busy in [true, false] {
        let mut app = working();
        if busy {
            app.start_run_for_test(0);
        }

        let buf = render_buffer(&app, 80, 16);
        for y in 0..16 {
            for x in 0..80 {
                let cell = buf.cell((x, y)).expect("cell");
                if is_shimmer(cell.fg) && cell.symbol() == "─" {
                    panic!("border must not carry an in-app shimmer at ({x},{y}), busy={busy}");
                }
            }
        }
    }
}
