//! Corpus generators for the transcript baseline bench. Each generator
//! returns a deterministic frame log of roughly the requested length, so a
//! bench run is reproducible across machines and commits. The three classes
//! cover the projection paths that dominate rebuild and fold cost: short
//! text turns, reasoning-heavy turns, and tool-call-plus-result pairs.

use houyicoder_protocol::frontend::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use houyicoder_tui::transcript::TranscriptFrame;
use serde_json::Value;

/// A corpus generator: takes a target frame count, returns the frame log.
type CorpusGen = fn(usize) -> Vec<TranscriptFrame>;

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

fn tool_call(id: &str, tool: &str, input: Value) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, tool)
            .raw_input(input)
            .status(ToolCallStatus::InProgress),
    ))
}

fn tool_result(id: &str, output: Value) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(output),
    )))
}

/// Alternating short user and agent text lines. Exercises the plain text
/// projection without tool folding or reasoning summary rows.
pub fn short_text(n: usize) -> Vec<TranscriptFrame> {
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                user_msg(&format!("user turn {i}"))
            } else {
                agent_msg(&format!("agent reply {i}"))
            }
        })
        .collect()
}

/// Reasoning chunks followed by a short agent reply per turn. Each turn is a
/// thought then a message, so the projection builds summary rows and measures
/// the reasoning path rather than only plain text.
pub fn reasoning_heavy(n: usize) -> Vec<TranscriptFrame> {
    (0..n)
        .map(|i| {
            let turn = i / 3;
            let phase = i % 3;
            match phase {
                0 => thought(&format!(
                    "turn {turn}: weighing the options and the tradeoffs"
                )),
                1 => thought(&format!(
                    "turn {turn}: the second consideration narrows the set"
                )),
                _ => agent_msg(&format!("turn {turn}: done")),
            }
        })
        .collect()
}

/// Tool-call plus result pairs. Every other frame starts a tool call and the
/// next completes it, so the fold scanner finds one group per pair and the
/// rebuild projects both the call and its result line.
pub fn tool_heavy(n: usize) -> Vec<TranscriptFrame> {
    (0..n)
        .map(|i| {
            let pair = i / 2;
            let id = format!("call-{pair}");
            if i % 2 == 0 {
                tool_call(
                    &id,
                    "bash",
                    serde_json::json!({ "command": format!("echo {pair}") }),
                )
            } else {
                tool_result(&id, serde_json::json!({ "stdout": format!("out {pair}") }))
            }
        })
        .collect()
}

/// The three corpus classes, so a bench can iterate them by name.
pub const CLASSES: &[(&str, CorpusGen)] = &[
    ("short-text", short_text),
    ("reasoning", reasoning_heavy),
    ("tool", tool_heavy),
];
