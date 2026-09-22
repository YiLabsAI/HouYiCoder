//! Regression tests for the jump-to-bottom label: a bright overlay on the
//! transcript's bottom row while the user is scrolled back from the tail,
//! showing the agent turns that landed since the scroll-away snapshot. Each
//! test states the invariant it pins.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus,
};

use crate::agent_message::{ServerEvent, SessionMessage};
use crate::composition;
use crate::records::TranscriptLine;
use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
use crate::state::{App, EventCursor, Screen};
use crate::test_harness::render_text;
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
    assert_eq!(app.new_turn_count().count, 0, "no new agent content yet");
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(agent_msg(
        "first response",
    ))));
    assert_eq!(
        app.new_turn_count().count,
        1,
        "one agent turn since snapshot"
    );
    // A tool call within the same turn does NOT start a new count.
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(tool_call_frame(
        "c1",
    ))));
    assert_eq!(
        app.new_turn_count().count,
        1,
        "tool call within a turn does not split the count"
    );
    // More agent text after the tool is still the same turn — still 1.
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(agent_msg(
        "continuing",
    ))));
    assert_eq!(
        app.new_turn_count().count,
        1,
        "later agent text in the same turn does not tick"
    );
}

/// The viewable window drops its oldest rows once it passes the cap. A
/// transcript-length baseline would saturate the count to zero after that
/// drop; the frame-index baseline survives, because frames truncate only on
/// rewind. Trim runs only at the tail, so the cap holds on resume, not
/// mid-scroll-back: the count is frame-based and tracks frames, not the
/// transcript length that changes while scrolled away.
#[test]
fn test_evicted_keeps_count() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    for i in 0..VIEWABLE_SCROLLBACK_CAP + 1 {
        app.push_transcript_line(TranscriptLine::User(format!("filler line {i}")));
    }
    assert_eq!(
        app.transcript.len(),
        VIEWABLE_SCROLLBACK_CAP,
        "the fill leaves the window at its cap"
    );
    let _out = render_text(&app, 80, 24);
    app.scroll_transcript_line_up(3);
    let _out = render_text(&app, 80, 24);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(agent_msg(
        "response",
    ))));
    assert_eq!(app.new_turn_count().count, 1);
    // One row past the cap while scrolled back: trim skips a scrolled-back
    // reader, so the line accumulates past the cap rather than draining. The
    // echo rows live in the frame log (frontend Echo frames), and the log is
    // bounded by bytes rather than by this row cap, so rebuild re-projects the
    // aged-out echo frame too: one extra row versus the old direct-push model.
    app.push_transcript_line(TranscriptLine::User("one past the cap".into()));
    assert_eq!(
        app.transcript.len(),
        VIEWABLE_SCROLLBACK_CAP + 3,
        "trim skips while scrolled back, so the line accumulates"
    );
    // The count is frame-based, so a length change during scroll-away does
    // not zero it: it still names the frames since the snapshot.
    assert_eq!(
        app.new_turn_count().count,
        1,
        "the frame-based count is independent of the transcript length"
    );
    // The cap holds at the tail: resume trims the accumulated rows back.
    app.scroll_transcript_follow_tail();
    assert_eq!(
        app.transcript.len(),
        VIEWABLE_SCROLLBACK_CAP,
        "resume-to-tail trims back to the cap"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "one past the cap")),
        "the newest row survives the resume trim"
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "filler line 1")),
        "the oldest row the resume trim drops leaves the window"
    );
    // Resume cleared the scroll-away snapshot, so the count returns to zero.
    assert_eq!(
        app.new_turn_count().count,
        0,
        "resume clears the scroll-away snapshot"
    );
}

/// A second scroll-away while already scrolled back must not reset the
/// baseline (null guard) — otherwise the count would drop on every wheel
/// notch after the first.
#[test]
fn test_rescroll_keeps_count() {
    let mut app = app_scrolled_back();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(agent_msg(
        "response",
    ))));
    let snapshot = app.unseen_since.expect("snapshot taken");
    // A second scroll-away: was already not following, so the null guard
    // keeps the original baseline.
    app.scroll_transcript_line_up(3);
    assert_eq!(
        app.unseen_since,
        Some(snapshot),
        "second scroll-away must not reset the baseline"
    );
    assert_eq!(app.new_turn_count().count, 1);
}

/// Clicking the label returns to the tail and clears the snapshot.
/// Hit-tested before the transcript surface so the click does not start a
/// drag-selection on the row under it.
#[test]
fn test_click_label_jumps() {
    let mut app = app_scrolled_back();
    // The idle-state text names the click affordance so the label reads as
    // clickable, not just as a status hint.
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Jump to bottom (click)"),
        "idle label should carry the (click) hint:\n{out}"
    );
    let rect = app.jump_to_bottom_rect.get();
    assert!(
        rect.height > 0 && rect.width > 0,
        "label visible when scrolled back"
    );
    let px = rect.x + rect.width / 2;
    let py = rect.y;
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), px, py),
    );
    assert!(
        app.transcript_scroll.is_following_tail(),
        "click returns to the tail"
    );
    assert!(
        app.unseen_since.is_none(),
        "click clears the scroll-away snapshot"
    );
}

/// A click on the blank cell beside the label must NOT jump — it falls
/// through to the transcript surface, since the hit rect is the label span
/// rather than the full row.
#[test]
fn test_label_side_falls_through() {
    let mut app = app_scrolled_back();
    let rect = app.jump_to_bottom_rect.get();
    assert!(
        rect.width < 80,
        "the hit rect is the label span, not the full row"
    );
    // Click the far-left cell of the label row, outside the label span.
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 0, rect.y),
    );
    assert!(
        !app.transcript_scroll.is_following_tail(),
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
        app.transcript_scroll.is_following_tail(),
        "short transcript scroll keeps follow-tail"
    );
    let _out = render_text(&app, 80, 24);
    let rect = app.jump_to_bottom_rect.get();
    assert_eq!(rect.height, 0, "no ghost label on a short transcript");
}

/// While following the tail (the default), the label is hidden.
#[test]
fn test_label_hidden_following() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    for i in 0..50 {
        app.system_line(format!("line {i}"));
    }
    let _out = render_text(&app, 80, 24);
    assert!(
        app.transcript_scroll.is_following_tail(),
        "default follows the tail"
    );
    let rect = app.jump_to_bottom_rect.get();
    assert_eq!(rect.height, 0, "label hidden while following the tail");
}

/// A single wheel-up notch from the tail must move the visible content (not
/// just surface the label). This is the full path — event -> line_up ->
/// render -> output — so a regression that breaks follow but leaves the
/// viewport pinned at the tail is caught here, not just at the offset
/// arithmetic.
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
        !app.transcript_scroll.is_following_tail(),
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

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// Deliver one frame through the production path.
fn pump(app: &mut App, frame: TranscriptFrame) {
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(frame)));
}

/// An anchor frame the byte budget evicted leaves the label counting the turns
/// the window still shows and reporting the count as a floor, rather than
/// reading zero new turns because the frames it stood on are gone.
#[test]
fn test_floor_count_after_drain() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    let text = "x".repeat(100);
    for i in 0..500 {
        pump(&mut app, user_msg(&format!("u{i} {text}")));
        pump(&mut app, agent_msg(&format!("a{i} {text}")));
    }
    // The user scrolls back from the tail: the anchor is the frame count then.
    app.unseen_since = Some(EventCursor::Local(1000));
    app.transcript.set_resident_byte_budget(1024);
    for i in 500..800 {
        pump(&mut app, user_msg(&format!("u{i} {text}")));
        pump(&mut app, agent_msg(&format!("a{i} {text}")));
    }
    let base = app.transcript.frame_window_start();
    let new = app.new_turn_count();
    assert!(
        base > 1000,
        "the drain evicted the anchor frame: base={base}"
    );
    assert!(
        new.count > 0,
        "the label counts the turns the window still shows: {new:?}"
    );
    assert!(new.is_lower_bound, "a gone anchor reports a floor");
    app.transcript_scroll.jump_to(0);
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("+ new message"),
        "a floor count renders with the + marker:\n{out}"
    );
}
