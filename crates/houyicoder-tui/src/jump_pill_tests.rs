//! Regression tests for the "jump to bottom" pill (bug #64): a bright
//! overlay on the transcript's bottom row when the user has scrolled back
//! from the tail, showing "N new messages" (agent-turn count) or "Jump to
//! bottom". Click returns to the tail. These pin the load-bearing
//! invariants:
//! - the count is agent turns (one per user message that produced agent
//!   text), not segments — prev_was_agent resets only on UserMessageChunk so
//!   a tool call within a turn does not split it; counting user frames
//!   instead would be dead in production (a user frame only arrives via
//!   submit, which clears the snapshot first);
//! - the count baseline is a frame index, not transcript.len(), so
//!   bound_scrollback eviction and transcript pops cannot silently zero it;
//! - a scroll-up on a short (one-viewport-or-less) transcript does not break
//!   follow-tail, so no ghost pill appears while the view is at the bottom;
//! - clicking the pill's centered label span (not the full row) returns to
//!   the tail; the blank cells on either side fall through to drag-select;
//! - a wheel-up notch from the tail moves the visible content on the first
//!   notch (not just surfaces the pill) — guards against a render/event
//!   regression that pins the viewport while flipping follow-tail.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus,
};

use crate::agent_message::AgentMessage;
use crate::composition;
use crate::state::Screen;
use crate::test_support::render_text;
use crate::transcript::TranscriptFrame;

fn agent_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// A tool-only frame (no adjacent agent text) — must not tick the pill count.
fn tool_call_frame(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, "grep").status(ToolCallStatus::InProgress),
    ))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// Fill the transcript with more than one viewport of system lines, render,
/// scroll back one line-step, and re-render so the pill rect is published.
fn app_scrolled_back() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    for i in 0..50 {
        app.system_line(format!("history line {i:02}"));
    }
    let _out = render_text(&app, 80, 24);
    app.scroll_transcript_line_up(3);
    let _out = render_text(&app, 80, 24);
    app
}

/// Agent content arriving while scrolled back increments the count by one
/// per user-to-assistant turn, not per response segment: a tool call within
/// one turn does not split it. Driven through the production path
/// (handle_agent_message), not a hand-built frame Vec.
#[test]
fn test_agent_count_rises() {
    let mut app = app_scrolled_back();
    assert_eq!(app.jump_pill_new_count(), 0, "no new agent content yet");
    app.handle_agent_message(AgentMessage::Frame(agent_msg("first response")));
    assert_eq!(
        app.jump_pill_new_count(),
        1,
        "one agent turn since snapshot"
    );
    // A tool call within the same turn does NOT start a new count.
    app.handle_agent_message(AgentMessage::Frame(tool_call_frame("c1")));
    assert_eq!(
        app.jump_pill_new_count(),
        1,
        "tool call within a turn does not split the count"
    );
    // More agent text after the tool is still the same turn — still 1.
    app.handle_agent_message(AgentMessage::Frame(agent_msg("continuing")));
    assert_eq!(
        app.jump_pill_new_count(),
        1,
        "later agent text in the same turn does not tick"
    );
}

/// bound_scrollback drains the oldest transcript rows past 4000. A
/// transcript-length baseline would saturate the count to zero after
/// eviction. The frame-index baseline is immune — frames only truncate on
/// rewind, so the count survives eviction.
#[test]
fn test_evicted_keeps_count() {
    let mut app = app_scrolled_back();
    app.handle_agent_message(AgentMessage::Frame(agent_msg("response")));
    assert_eq!(app.jump_pill_new_count(), 1);
    for i in 0..5000 {
        app.system_line(format!("filler line {i}"));
    }
    assert_eq!(
        app.jump_pill_new_count(),
        1,
        "eviction must not zero the frame-based count"
    );
}

/// A second scroll-away while already scrolled back must not reset the
/// baseline (null guard) — otherwise the count would drop on every wheel
/// notch after the first.
#[test]
fn test_rescroll_keeps_count() {
    let mut app = app_scrolled_back();
    app.handle_agent_message(AgentMessage::Frame(agent_msg("response")));
    let snapshot = app.scrolled_from_frame.expect("snapshot taken");
    // A second scroll-away: was already not following, so the null guard
    // keeps the original baseline.
    app.scroll_transcript_line_up(3);
    assert_eq!(
        app.scrolled_from_frame,
        Some(snapshot),
        "second scroll-away must not reset the baseline"
    );
    assert_eq!(app.jump_pill_new_count(), 1);
}

/// Clicking the pill's label span returns to the tail and clears the
/// snapshot. Hit-tested before the transcript surface so the click does not
/// start a drag-selection on the row under it.
#[test]
fn test_click_pill_jumps() {
    let mut app = app_scrolled_back();
    let pill = app.jump_pill_rect.get();
    assert!(
        pill.height > 0 && pill.width > 0,
        "pill visible when scrolled back"
    );
    let px = pill.x + pill.width / 2;
    let py = pill.y;
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), px, py),
    );
    assert!(
        app.transcript_scroll.follow_tail,
        "click returns to the tail"
    );
    assert!(
        app.scrolled_from_frame.is_none(),
        "click clears the scroll-away snapshot"
    );
}

/// A click on the blank cell to the side of the pill label must NOT jump — it
/// falls through to the transcript surface (the pill rect is the label span,
/// not the full row).
#[test]
fn test_pill_side_falls_through() {
    let mut app = app_scrolled_back();
    let pill = app.jump_pill_rect.get();
    assert!(
        pill.width < 80,
        "pill rect is the label span, not the full row"
    );
    // Click the far-left cell of the pill row, outside the label span.
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 0, pill.y),
    );
    assert!(
        !app.transcript_scroll.follow_tail,
        "click beside the label must not jump to the tail"
    );
}

/// A scroll-up on a transcript that fits one viewport or less must not break
/// follow-tail (max_top == 0 guard), so no ghost pill appears while the view
/// is already at the bottom.
#[test]
fn test_short_no_pill() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.system_line("only one line");
    let _out = render_text(&app, 80, 24);
    app.scroll_transcript_line_up(3);
    assert!(
        app.transcript_scroll.follow_tail,
        "short transcript scroll keeps follow-tail"
    );
    let _out = render_text(&app, 80, 24);
    let pill = app.jump_pill_rect.get();
    assert_eq!(pill.height, 0, "no ghost pill on a short transcript");
}

/// While following the tail (the default), the pill is hidden.
#[test]
fn test_pill_hidden_following() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    for i in 0..50 {
        app.system_line(format!("line {i}"));
    }
    let _out = render_text(&app, 80, 24);
    assert!(
        app.transcript_scroll.follow_tail,
        "default follows the tail"
    );
    let pill = app.jump_pill_rect.get();
    assert_eq!(pill.height, 0, "pill hidden while following the tail");
}

/// A single wheel-up notch from the tail must move the visible content (not
/// just surface the pill). This is the full path — event -> line_up -> render
/// -> output — so a regression that breaks follow but leaves the viewport
/// pinned at the tail is caught here, not just at the offset arithmetic.
#[test]
fn test_wheel_moves_content() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    for i in 0..50 {
        app.system_line(format!("history line {i:02}"));
    }
    let out_before = render_text(&app, 80, 24);
    let total = app.transcript_display_rows();
    let cap = app.transcript_scroll.cap.get();
    let top_before = app.transcript_scroll.top_offset(total);
    // Wheel up one notch in the middle of the transcript area.
    crate::app::handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 40, 12));
    let top_after = app.transcript_scroll.top_offset(total);
    assert!(
        !app.transcript_scroll.follow_tail,
        "wheel up breaks follow-tail"
    );
    assert!(
        top_after < top_before,
        "first wheel notch must move the top toward older rows ({top_before} -> {top_after}, cap={cap})"
    );
    let out_after = render_text(&app, 80, 24);
    assert_ne!(
        out_before, out_after,
        "rendered content must change after the first wheel notch"
    );
}
