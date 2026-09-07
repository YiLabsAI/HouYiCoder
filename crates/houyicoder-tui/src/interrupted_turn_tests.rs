//! Tests whether an interrupted turn remains submitted or is restored for
//! editing. Covers missing context, assistant output, tool activity, reasoning,
//! and the boundary established by the latest user message.

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
fn test_missing_user_preserves() {
    // Missing frame context preserves the turn conservatively.
    assert!(should_preserve_interrupted_turn(&[]));
}

#[test]
fn test_user_only_restores() {
    // A submission without assistant output may be restored.
    assert!(!should_preserve_interrupted_turn(&[user_msg("hi")]));
}

#[test]
fn test_agent_output_preserves() {
    // Assistant output preserves the submitted turn.
    let frames = vec![user_msg("hi"), agent_msg("hello back")];
    assert!(should_preserve_interrupted_turn(&frames));
}

#[test]
fn test_empty_output_restores() {
    // An empty boundary flush does not preserve the turn.
    let frames = vec![user_msg("hi"), agent_msg("")];
    assert!(!should_preserve_interrupted_turn(&frames));
}

#[test]
fn test_tool_call_preserves() {
    // A tool call preserves the submitted turn.
    let frames = vec![user_msg("hi"), tool_call("c1", "bash")];
    assert!(should_preserve_interrupted_turn(&frames));
}

#[test]
fn test_lone_result_restores() {
    // A lone tool result is not model-authored output and may be restored.
    let frames = vec![
        user_msg("hi"),
        tool_result("c1", serde_json::json!({"error": "interrupted by user"})),
    ];
    assert!(!should_preserve_interrupted_turn(&frames));
}

#[test]
fn test_thought_preserves() {
    // Reasoning output preserves the submitted turn.
    let frames = vec![user_msg("hi"), thought("thinking")];
    assert!(should_preserve_interrupted_turn(&frames));
}

#[test]
fn test_last_user_controls() {
    // Only output after the latest user message controls restoration.
    let frames = vec![user_msg("first"), agent_msg("old"), user_msg("second")];
    assert!(!should_preserve_interrupted_turn(&frames));
}
