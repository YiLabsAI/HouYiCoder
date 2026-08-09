//! Seal tests for the per-frame transcript rebuild (the TUI half of the
//! tool-batch-display fix). route B ships durable frames mid-run; the Frame
//! arm calls rebuild_transcript per-frame so each frame renders immediately,
//! not only at the next PermissionAsk/Done. rebuild_transcript's dual cursor
//! (sealed_frames_end + sealed_transcript_len) bounds per-frame cost to the
//! current turn; this file pins the seal's behavior + its edge cases.

use crate::composition;
use crate::records::TranscriptLine;
use crate::state::App;
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::json;

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn tool_call(id: &str, tool: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, tool)
            .raw_input(json!({}))
            .status(ToolCallStatus::InProgress),
    ))
}
fn tool_result(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(json!({"ok": true})),
    )))
}
fn todo_write_frame(id: &str, todos: &[(&str, &str)]) -> TranscriptFrame {
    let arr = todos
        .iter()
        .map(|(c, s)| json!({"content": c, "status": s}))
        .collect::<Vec<_>>();
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, "todo_write")
            .raw_input(json!({"todos": arr}))
            .status(ToolCallStatus::InProgress),
    ))
}

/// A fresh stub App with a clean transcript + frame log (the composition::app
/// default ships a demo transcript that would mask the projection under test).
fn fresh_app() -> App {
    let mut app = composition::app();
    app.transcript.clear();
    app.frames.clear();
    app.sealed_frames_end = 0;
    app.sealed_transcript_len = 0;
    app
}

fn pump(app: &mut App, frame: TranscriptFrame) {
    use crate::agent_message::AgentMessage;
    app.handle_agent_message(AgentMessage::Frame(frame));
}

/// The ToolCall frame projects into the transcript the moment it arrives —
/// no PermissionAsk or Done needed. Before the per-frame rebuild the
/// transcript only updated at the ask, so a tool call stayed invisible
/// mid-run. RED before step1 (transcript empty of the call), GREEN after.
#[test]
fn test_frame_projects_without_ask() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    let has_call = app.transcript.iter().any(|l| {
        matches!(
            l,
            TranscriptLine::Tool {
                tool,
                name,
                ..
            } if *tool == "glob" && name != "result"
        )
    });
    assert!(
        has_call,
        "ToolCall projects without an ask: {:?}",
        app.transcript
    );
}

/// Pair-safety: a ToolCall whose ToolCallUpdate lands AFTER a turn boundary
/// (TurnAborted projects as UserMessageChunk) must not be split across the
/// seal — the call in the prefix, the result in the tail would orphan. The
/// seal falls back to the UserMessageChunk before the split so the pair
/// stays together in the tail + re-projects paired. Assert the result row
/// sits immediately after its call (adjacency = paired, not orphaned).
#[test]
fn test_pair_safe_no_orphan() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    // A turn boundary (TurnAborted projects as UserMessageChunk) lands between
    // the call and its result — the shape that would split the pair.
    pump(&mut app, user_msg("(interrupted)"));
    pump(&mut app, tool_result("c1"));
    let call_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name != "result" && call_id == "c1"))
        .expect("call row present");
    let result_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name == "result" && call_id == "c1"))
        .expect("result row present");
    assert_eq!(
        result_at,
        call_at + 1,
        "result is adjacent to its call (paired, not orphaned): {:?}",
        app.transcript
    );
}

/// Rewind truncates the frame log below the seal; the seal must invalidate +
/// re-project from the truncated frames (no stale lines from the dropped
/// frames survive in the transcript).
#[test]
fn test_rewind_resets_seal() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob"));
    pump(&mut app, user_msg("second"));
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("second"))),
        "second user echo present before rewind"
    );
    app.rewind_to_last_user_input();
    let stale = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("second")));
    assert!(
        !stale,
        "rewind dropped the second echo (no stale line): {:?}",
        app.transcript
    );
    let kept = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("go")));
    assert!(
        kept,
        "first user echo survives rewind: {:?}",
        app.transcript
    );
}

/// A batch of frames landing between rebuilds (the future replay path, or any
/// path that pushes two frames before one rebuild) must not double the turn's
/// lines. The seal's sealed_transcript_len is the prefix projection length
/// (not transcript.len()), so the incremental path replaces the tail region
/// rather than appending to a prefix that already contains it. Pins the
/// consistency the pair-safety + per-frame tests can't reach (they pump one
/// frame per rebuild, which masks a stale seal by coincidence).
#[test]
fn test_batch_replay_no_duplicate() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    // Push two frames directly without an intervening rebuild (a batch), then
    // rebuild once — the incremental path must handle the non-empty suffix.
    app.frames.push(tool_call("c1", "glob"));
    app.frames.push(tool_result("c1"));
    app.rebuild_transcript();
    let calls = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if *name != "result" && call_id == "c1"))
        .count();
    let results = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if *name == "result" && call_id == "c1"))
        .count();
    assert_eq!(
        calls, 1,
        "no duplicate call row after batch+rebuild: {:?}",
        app.transcript
    );
    assert_eq!(
        results, 1,
        "no duplicate result row after batch+rebuild: {:?}",
        app.transcript
    );
}

/// todo_write renders only via the checklist widget, not as a transcript tool
/// row. Without the skip the tool call would double-render (a chip + the
/// widget). The frame stays in the log so todo_view still parses it. RED
/// before step3-lite (a todo_write Tool row appears), GREEN after.
#[test]
fn test_todo_write_skips_row() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, todo_write_frame("c1", &[("task one", "pending")]));
    let has_tool_row = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::Tool { tool, .. } if *tool == "todo_write"));
    assert!(
        !has_tool_row,
        "todo_write must not render a transcript tool row (the widget owns it): {:?}",
        app.transcript
    );
}

/// A hanging ToolCall (no ToolCallUpdate anywhere — the cross-process resume
/// norm, where the prior run was interrupted before the tool completed) must
/// NOT trigger the pair-safety fallback. Without the fix the fallback bottoms
/// out at 0 (every candidate unpaired) → the whole session re-projects fully
/// per frame. With the fix the seal lands at the real last UserMessageChunk + 1
/// (a hanging call has nothing to orphan). Pins the perf regression the
/// per-frame/correctness tests can't reach.
#[test]
fn test_hanging_call_keeps_seal() {
    let mut app = fresh_app();
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "glob")); // hanging — no tool_result ever
    pump(&mut app, user_msg("second"));
    // The seal lands after "second" (index 3), not collapsed to 1 by the
    // hanging c1 (no result to orphan → no fallback past it).
    assert_eq!(
        app.turn_seal_point(),
        3,
        "hanging call must not collapse the seal"
    );
}
