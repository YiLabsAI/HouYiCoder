//! The state a window hands on holds what a turn's summary needs and nothing
//! whose size a frame decides: the tests here hold it to that ceiling, for the
//! names at their bounds and for the ones past them.

use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus,
};

use super::super::{RowNames, TurnFold};
use super::{MAX_CALL_ID, MAX_TOOL_NAME, NAMED_TOOLS, PENDING_CALLS, TurnCheckpoint};
use crate::transcript::TranscriptFrame;

/// The most the state holds of what a frame carried: the names it keeps, which
/// are the tool names and call ids a frame supplied. The identities it holds
/// are the host's own stamps, so they are not part of this ceiling.
const CARRIED_CEILING: usize =
    NAMED_TOOLS * MAX_TOOL_NAME + PENDING_CALLS * (MAX_CALL_ID + MAX_TOOL_NAME);

/// A string of exactly this length, distinct per index, standing in for a tool
/// name or a call id a frame supplies.
fn sized(prefix: &str, len: usize, at: usize) -> String {
    let mut s = format!("{prefix}{at}");
    s.push_str(&"x".repeat(len - s.len()));
    s
}

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

fn tool_call(call_id: &str, tool: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(call_id, tool).status(ToolCallStatus::InProgress),
    ))
}

fn thought(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// Fold a window that opens the turn and holds all of it, and report the state
/// it hands on.
fn state_of(frames: &[TranscriptFrame]) -> TurnCheckpoint {
    let mut fold = TurnFold::new(
        frames,
        0..frames.len(),
        RowNames::Position { abs_base: 0 },
        None,
    );
    for (at, frame) in frames.iter().enumerate() {
        fold.note(at, frame);
    }
    fold.checkpoint()
}

/// The bytes the state holds of what a frame carried.
fn carried_bytes(state: &TurnCheckpoint) -> usize {
    let names: usize = state.tool_counts.named().iter().map(|(n, _)| n.len()).sum();
    let calls: usize = state
        .pending_calls
        .calls
        .iter()
        .map(|(id, tool)| id.len() + tool.len())
        .sum();
    names + calls
}

#[test]
fn test_checkpoint_size_stays_bounded() {
    let mut frames = vec![user_msg("go")];
    for at in 0..PENDING_CALLS {
        let tool = if at < NAMED_TOOLS {
            sized("t", MAX_TOOL_NAME, at)
        } else {
            sized("u", MAX_TOOL_NAME, at)
        };
        frames.push(tool_call(&sized("c", MAX_CALL_ID, at), &tool));
    }
    let state = state_of(&frames);
    assert_eq!(
        state.pending_calls.calls.len(),
        PENDING_CALLS,
        "every call within the bound is kept"
    );
    assert_eq!(
        state.tool_counts.named().len(),
        NAMED_TOOLS,
        "the state names as many tools as it will name"
    );
    assert_eq!(state.calls, PENDING_CALLS as u32, "the total stays exact");
    assert_eq!(
        state.tool_counts.other(),
        (PENDING_CALLS - NAMED_TOOLS) as u32,
        "the tools past the named bound count without being named"
    );
    assert_eq!(
        carried_bytes(&state),
        CARRIED_CEILING,
        "a turn that fills every bound holds exactly the ceiling"
    );
    // The bounds as the state declares them, written out: the composition
    // above follows a changed bound silently, so the absolute number is what
    // makes growing one a decision rather than a side effect.
    assert_eq!(
        CARRIED_CEILING, 9_216,
        "the ceiling is the two bounds filled"
    );
}

#[test]
fn test_checkpoint_rejects_long_names() {
    // The id sits well inside its bound, so the tool name alone decides this
    // call: an assertion that either bound could explain would not say which.
    let over_name = sized("t", MAX_TOOL_NAME + 1, 0);
    let frames = vec![
        user_msg("go"),
        tool_call("c1", &over_name),
        thought(&"w".repeat(200_000)),
    ];
    let state = state_of(&frames);
    assert!(
        state.tool_counts.named().is_empty(),
        "a name longer than the state keeps is not kept"
    );
    assert_eq!(
        state.tool_counts.other(),
        1,
        "the call still counts toward the turn"
    );
    assert_eq!(state.calls, 1, "the call still counts toward the summary");
    assert!(
        state.pending_calls.tool("c1").is_none(),
        "the call is not kept, so a window that reads on cannot name it"
    );
    assert_eq!(
        state.reasoning_chars, 200_000,
        "reasoning is reported by its size"
    );
    assert_eq!(carried_bytes(&state), 0, "the long tool name is not held");
}

#[test]
fn test_checkpoint_rejects_long_id() {
    // The tool name here is short enough to be named and kept, so an unkept
    // call can only be the id's doing.
    let long_id = sized("c", 5_000, 0);
    let frames = vec![user_msg("go"), tool_call(&long_id, "read")];
    let state = state_of(&frames);
    assert_eq!(
        state.tool_counts.named(),
        [("read".to_string(), 1)],
        "the tool is named even though the call is not kept"
    );
    assert_eq!(state.calls, 1, "the call still counts toward the summary");
    assert!(
        state.pending_calls.tool(&long_id).is_none(),
        "an id past the bound leaves the call unkept"
    );
    assert_eq!(
        carried_bytes(&state),
        "read".len(),
        "the id is not held; only the short tool name is"
    );
}

#[test]
fn test_checkpoint_limits_pending_calls() {
    let mut frames = vec![user_msg("go")];
    for at in 0..PENDING_CALLS + 1 {
        frames.push(tool_call(&sized("c", 8, at), &sized("t", 8, at)));
    }
    let state = state_of(&frames);
    assert_eq!(
        state.pending_calls.calls.len(),
        PENDING_CALLS,
        "the state keeps as many calls as it keeps"
    );
    assert_eq!(
        state.calls,
        (PENDING_CALLS + 1) as u32,
        "the call past the bound still counts toward the summary"
    );
    assert!(
        state
            .pending_calls
            .tool(&sized("c", 8, PENDING_CALLS))
            .is_none(),
        "the call past the bound is not kept, so a result for it renders unnamed"
    );
}
