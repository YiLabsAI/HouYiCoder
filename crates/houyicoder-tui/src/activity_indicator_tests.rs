//! Working and Thinking indicator presentation tests.

#![cfg(test)]

use crate::composition;
use crate::state::Screen;
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

#[test]
fn test_spinner_verb_reflects_phase() {
    // The spinner verb tracks the active stream phase.
    let mut app = working();
    app.agent_busy = true;
    app.run_started = Some(std::time::Instant::now());
    // Reasoning streaming ⇒ Thinking.
    app.live_reasoning_text = "pondering the task".to_string();
    app.live_block = crate::state::enums::LiveBlock::Thinking;
    let out = render_text(&app, 80, 12);
    assert!(
        out.contains("Thinking"),
        "reasoning phase should show Thinking:\n{out}"
    );
    // No reasoning streaming ⇒ Working.
    app.live_reasoning_text.clear();
    app.live_block = crate::state::enums::LiveBlock::None;
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
    app.agent_busy = true;
    app.run_started = Some(std::time::Instant::now());
    // Reasoning streamed first, then assistant text takes over.
    app.live_reasoning_text = "pondered".to_string();
    app.live_assistant_text = "Here is the answer".to_string();
    app.live_block = crate::state::enums::LiveBlock::Responding;
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

#[test]
fn test_busy_border_stays_plain() {
    // Terminal progress owns the processing signal; the pane border stays plain.
    use ratatui::style::Color;
    let is_shimmer = |c: Color| matches!(c, Color::Rgb(0, g, b) if g == b && g > 0);
    for busy in [true, false] {
        let mut app = working();
        app.agent_busy = busy;
        app.run_started = busy.then(std::time::Instant::now);
        let buf = crate::test_support::render_buffer(&app, 80, 16);
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
