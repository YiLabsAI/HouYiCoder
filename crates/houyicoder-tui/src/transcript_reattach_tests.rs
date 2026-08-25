//! Late-arriving tool results reattach to their call in the transcript view.

use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

use crate::records::TranscriptLine;
use crate::transcript::{TranscriptFrame, transcript_from_frames};

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn thought(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn tool_call(id: &str, tool: &str, input: serde_json::Value) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, tool)
            .raw_input(input)
            .status(ToolCallStatus::InProgress),
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

/// Late-arriving results (ToolCallUpdate lands AFTER the ToolCall frame, with
/// a thought between) must reposition adjacent to their call, not stay at the
/// arrival position. Before the fix, a Glob call followed by an Update call +
/// a thought + Update's diff result + Glob's result rendered with the Glob
/// result detached from the Glob chip and the thought interleaving between
/// call and result. The reposition pass attaches each late result right after
/// its call, preserving call+result adjacency and input call order.
#[test]
fn test_late_results_to_call() {
    let frames = vec![
        user_msg("go"),
        tool_call("c1", "glob", serde_json::json!({"pattern": "*.rs"})),
        tool_call(
            "c2",
            "edit",
            serde_json::json!({"path": "f.rs", "old_string": "a", "new_string": "b"}),
        ),
        thought("let me think"),
        // Results arrive AFTER the thought, in reverse call order.
        tool_result(
            "c2",
            serde_json::json!({"diff": "+b\n-a", "occurrences_replaced": 1}),
        ),
        tool_result(
            "c1",
            serde_json::json!({"filenames": ["f.rs"], "num_files": 1}),
        ),
    ];
    let t = transcript_from_frames(&frames);
    let c1_call = t
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { call_id, name, .. } if call_id == "c1" && name != "result"))
        .expect("c1 call row");
    let c1_result = t
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { call_id, name, .. } if call_id == "c1" && name == "result"))
        .expect("c1 result row");
    let c2_call = t
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { call_id, name, .. } if call_id == "c2" && name != "result"))
        .expect("c2 call row");
    let c2_result = t
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { call_id, name, .. } if call_id == "c2" && name == "result"))
        .expect("c2 result row");
    // Each result sits immediately after its call (call+result adjacency).
    assert_eq!(
        c1_result,
        c1_call + 1,
        "c1 result adjacent to c1 call: {t:?}"
    );
    assert_eq!(
        c2_result,
        c2_call + 1,
        "c2 result adjacent to c2 call: {t:?}"
    );
    // Input call order preserved (c1 before c2).
    assert!(c1_call < c2_call, "call order c1 before c2: {t:?}");
    // The thought does NOT land between a call and its result.
    let thought_idx = t
        .iter()
        .position(|l| matches!(l, TranscriptLine::Thinking { .. }))
        .expect("thought row");
    assert!(
        thought_idx > c2_result,
        "thought must not interleave between call and result: {t:?}"
    );
    // The Glob result carries the Glob (filenames) body, not the Update diff.
    if let TranscriptLine::Tool { body, .. } = &t[c1_result] {
        assert!(body.contains("f.rs"), "c1 result is the glob body: {body}");
        assert!(
            !body.contains("removed"),
            "c1 result is not the edit diff: {body}"
        );
    }
}

/// A real Edit result carries a "diff" field, so result_line sets is_diff true
/// and the call+result land adjacent. compute_fold_groups must EXEMPT the edit
/// (render its own Update chip + diff body), not fold it into a cross-tool
/// "Edited 1 file" summary. A regression that drops is_diff or detaches the
/// result from its call would fold the edit and bury the diff.
#[test]
fn test_edit_diff_skips_fold() {
    let frames = vec![
        tool_call(
            "c1",
            "edit",
            serde_json::json!({"path": "README.md", "old_string": "a", "new_string": "b"}),
        ),
        tool_result(
            "c1",
            serde_json::json!({
                "path": "README.md",
                "diff": "@@ -1 +1 @@\n-a\n+b\n",
                "occurrences_replaced": 1,
                "bytes": 2,
            }),
        ),
    ];
    let lines = transcript_from_frames(&frames);
    assert_eq!(lines.len(), 2, "call + result, got {lines:?}");
    assert!(matches!(
        &lines[1],
        TranscriptLine::Tool { name, is_diff, .. }
        if name == "result" && *is_diff
    ));
    let groups = crate::fold::compute_fold_groups(&lines, false);
    assert!(
        groups.is_empty(),
        "edit with diff must not fold, got {groups:?}"
    );
}

/// A multi-tool turn (read, edit, grep) with results arriving after all calls
/// (the parallel-batch shape). The edit must still render its own expanded
/// Update chip + diff — never folded into a cross-tool summary — and its
/// result row must stay adjacent to its call so call_has_diff_result (i+1)
/// holds.
#[test]
fn test_edit_diff_survives_batch() {
    let frames = vec![
        tool_call("c1", "read", serde_json::json!({"path": "a.md"})),
        tool_call(
            "c2",
            "edit",
            serde_json::json!({"path": "a.md", "old_string": "x", "new_string": "y"}),
        ),
        tool_call("c3", "grep", serde_json::json!({"pattern": "y"})),
        tool_result("c1", serde_json::json!({"path": "a.md", "content": "x"})),
        tool_result(
            "c2",
            serde_json::json!({
                "path": "a.md",
                "diff": "@@ -1 +1 @@\n-x\n+y\n",
                "occurrences_replaced": 1,
                "bytes": 2,
            }),
        ),
        tool_result("c3", serde_json::json!({"num_matches": 1})),
    ];
    let lines = transcript_from_frames(&frames);
    let edit_idx = lines
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { tool, .. } if tool == "edit"))
        .expect("edit call row present");
    assert!(
        matches!(
            &lines[edit_idx + 1],
            TranscriptLine::Tool { name, is_diff, .. }
            if name == "result" && *is_diff
        ),
        "edit result must be adjacent + is_diff, got {:?}",
        &lines[edit_idx..]
    );
    let groups = crate::fold::compute_fold_groups(&lines, false);
    assert!(
        !groups.iter().any(|g| {
            lines[g.start..g.end]
                .iter()
                .any(|l| matches!(l, TranscriptLine::Tool { tool, .. } if tool == "edit"))
        }),
        "edit must not be inside any fold group, got {groups:?}"
    );
}

// A duplicate call_id (empty or reused within one response) is minted unique
// at the provider boundary before any frame reaches the transcript, so the
// shape the old reused_id_no_swap test pinned — two calls sharing one id with
// results arriving in completion order — cannot be produced by any real path.
// Deleted rather than kept ignored: an ignored test of an unproducable shape
// rots silently, and its ignore reason ("results append in call order") was
// already wrong (they append in completion order). The pairing invariant now
// lives at the mint; see take_update in transcript.rs for the pointer.

/// An agent-tool result (carries agentId in its structured output) renders as
/// an inline Subagent fold-group line, not a generic tool result row. The
/// child session id + summary come from the result; the subagent_type comes
/// from the call input. folded_transcript starts empty (fetched on expand).
/// Pins the inline-fold wiring so a refactor that reverts to a plain result
/// row for delegations fails here.
#[test]
fn test_agent_result_renders_subagent() {
    let frames = vec![
        user_msg("find the auth module"),
        tool_call(
            "c1",
            "agent",
            serde_json::json!({
                "description": "find auth",
                "prompt": "locate the auth module",
                "subagent_type": "explore",
            }),
        ),
        tool_result(
            "c1",
            serde_json::json!({
                "status": "completed",
                "content": "auth module is at crates/auth/src/lib.rs",
                "agentId": "child-sess-42",
                "result_ref": "child-sess-42",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "reasoning_tokens": 0,
                },
            }),
        ),
    ];
    let lines = transcript_from_frames(&frames);
    let sub = lines
        .iter()
        .find(|l| matches!(l, TranscriptLine::Subagent { .. }))
        .expect("agent delegation produces a Subagent line");
    match sub {
        TranscriptLine::Subagent {
            child_sid,
            subagent_type,
            summary,
            prompt: _,
            folded_transcript,
            color: _,
        } => {
            assert_eq!(child_sid, "child-sess-42");
            assert_eq!(subagent_type, "explore");
            assert_eq!(summary, "auth module is at crates/auth/src/lib.rs");
            assert!(folded_transcript.is_empty(), "starts collapsed");
        }
        _ => unreachable!(),
    }
}

/// A non-agent tool result (no agentId field) does NOT render as a Subagent
/// line — it stays a plain tool result row. Pins the agentId detection so a
/// refactor that mis-detects other tools as delegations fails here.
#[test]
fn test_non_agent_skips_subagent() {
    let frames = vec![
        tool_call("c1", "bash", serde_json::json!({"command": "ls"})),
        tool_result("c1", serde_json::json!({"stdout": "a.rs\nb.rs"})),
    ];
    let lines = transcript_from_frames(&frames);
    assert!(
        lines
            .iter()
            .all(|l| !matches!(l, TranscriptLine::Subagent { .. })),
        "a bash result must not render as a Subagent line"
    );
}
