//! Silent-success bash commands render "done" instead of "(no output)".

use houyicoder_protocol::frontend::session_update::{
    SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

use crate::records::TranscriptLine;
use crate::transcript::{TranscriptFrame, transcript_from_frames};

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

fn result_body(lines: &[TranscriptLine]) -> String {
    match &lines[1] {
        TranscriptLine::Tool { name, body, .. } if name == "result" => body.clone(),
        other => panic!("expected result row, got {other:?}"),
    }
}

/// A silent bash command (mv, cp, rm, mkdir, ...) that succeeds with no
/// output renders "done" as the result body, not the empty string that
/// would become the "(no output)" placeholder. The user sees an explicit
/// success signal for a command whose output is silence by design.
#[test]
fn test_silent_bash_renders_done() {
    let frames = vec![
        tool_call("c1", "bash", serde_json::json!({"command": "mv a b"})),
        tool_result("c1", serde_json::json!({"stdout": "", "exit_code": 0})),
    ];
    let lines = transcript_from_frames(&frames);
    let body = result_body(&lines);
    assert_eq!(
        body, "done",
        "silent command should render done, got {body}"
    );
}

/// A non-silent bash command (echo) with empty stdout does NOT render
/// "done" — it falls through to the empty body, which the marker layer
/// turns into "(no output)". echo producing no output is unexpected.
#[test]
fn test_non_silent_bash_empty() {
    let frames = vec![
        tool_call("c1", "bash", serde_json::json!({"command": "echo"})),
        tool_result("c1", serde_json::json!({"stdout": "", "exit_code": 0})),
    ];
    let lines = transcript_from_frames(&frames);
    let body = result_body(&lines);
    assert!(
        body.is_empty(),
        "non-silent command with no output should NOT render done, got {body}"
    );
}

/// A silent bash command that produces stdout does NOT render "done" —
/// the real output surfaces instead. "done" is only for silence.
#[test]
fn test_silent_bash_with_output() {
    let frames = vec![
        tool_call("c1", "bash", serde_json::json!({"command": "mv -v a b"})),
        tool_result(
            "c1",
            serde_json::json!({"stdout": "renamed a -> b", "exit_code": 0}),
        ),
    ];
    let lines = transcript_from_frames(&frames);
    let body = result_body(&lines);
    assert_eq!(body, "renamed a -> b");
}
