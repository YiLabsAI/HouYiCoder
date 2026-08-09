//! Unit tests for run_produced_real_content — the frame-scan predicate that
//! decides auto-restore on interrupt: an aborted run restores the input only
//! when the model produced no real content after the last user message. Covers
//! the empty, no-user-message, message (empty + non-empty), tool-call,
//! tool-call-update-only, thought, and two-user-message cases. Streaming
//! deltas are not on the wire (they ride the live sink), so the predicate
//! scans wire frames only. Split out of run_control_tests.rs for the size gate.

use super::*;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn agent_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn thought(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn tool_call(id: &str, tool: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, tool).status(ToolCallStatus::InProgress),
    ))
}
fn tool_result(id: &str, output: serde_json::Value) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(output),
    )))
}

#[test]
fn test_real_content_empty_true() {
    // A failed replay unwraps to default (empty). Be conservative: return
    // true so a garbled stream never wrongly resets the input box.
    assert!(run_produced_real_content(&[]));
}

#[test]
fn test_content_user_only_false() {
    // Just the user message, no assistant output: the model never started
    // producing. Restore should fire.
    assert!(!run_produced_real_content(&[user_msg("hi")]));
}

#[test]
fn test_content_agent_chunk_counts() {
    // A non-empty agent message chunk after the user input means the model
    // produced a reply; the turn stands, do not restore.
    let frames = vec![user_msg("hi"), agent_msg("hello back")];
    assert!(run_produced_real_content(&frames));
}

#[test]
fn test_content_skips_empty_chunk() {
    // The flush path appends an empty agent message even when no text was
    // produced (empty boundary flush). An empty chunk must NOT count as real
    // content, otherwise a bare abort would look like the model produced
    // something and restore would never fire.
    let frames = vec![user_msg("hi"), agent_msg("")];
    assert!(!run_produced_real_content(&frames));
}

#[test]
fn test_content_tool_call_counts() {
    // A tool call after the user input means the model decided to act; the
    // turn stands.
    let frames = vec![user_msg("hi"), tool_call("c1", "bash")];
    assert!(run_produced_real_content(&frames));
}

#[test]
fn test_content_skips_tool_result() {
    // A tool-call update (a result landing) on its own is harness-or-engine
    // output, not model-authored content; it does not count as real content,
    // so restore still fires.
    let frames = vec![
        user_msg("hi"),
        tool_result("c1", serde_json::json!({"error": "interrupted by user"})),
    ];
    assert!(!run_produced_real_content(&frames));
}

#[test]
fn test_real_content_thought_counts() {
    // A thought chunk after the user input means the model started producing;
    // the turn stands.
    let frames = vec![user_msg("hi"), thought("thinking")];
    assert!(run_produced_real_content(&frames));
}

#[test]
fn test_content_uses_last_user() {
    // When there are two user messages, only the tail after the LAST one
    // matters (that is the run being aborted). An agent chunk before the
    // last user message does not rescue the turn.
    let frames = vec![user_msg("first"), agent_msg("old"), user_msg("second")];
    assert!(!run_produced_real_content(&frames));
}
