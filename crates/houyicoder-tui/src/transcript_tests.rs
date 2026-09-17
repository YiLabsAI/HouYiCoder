use super::*;
use houyicoder_protocol::frontend::session_update::{
    ToolCall, ToolCallUpdate, ToolCallUpdateFields,
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

#[test]
fn test_frames_map_to_transcript() {
    let frames = vec![
        user_msg("hi"),
        agent_msg("hello back"),
        tool_call("c1", "bash", serde_json::json!({"command": "ls"})),
        tool_result("c1", serde_json::json!({"stdout": "file.txt"})),
    ];
    let lines = transcript_from_frames(&frames);
    assert_eq!(lines.len(), 4);
    assert!(matches!(lines[0], TranscriptLine::User(ref s) if s == "hi"));
    assert!(matches!(
        lines[1],
        TranscriptLine::Agent(ref s) if s == "hello back"
    ));
    assert!(matches!(
        lines[2],
        TranscriptLine::Tool { ref name, .. } if name == "bash"
    ));
    assert!(matches!(
        lines[3],
        TranscriptLine::Tool { ref name, .. } if name == "result"
    ));
}

/// A status-only update carries no raw_output. One frame per status flip
/// used to render one "result" row each — a phantom red "Read 0 lines"
/// per flip (bug-log #28).
fn status_update(id: &str, status: ToolCallStatus) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new().status(status),
    )))
}

#[test]
fn test_result_row_folds_updates() {
    // Several update frames for one call (status flip + output + status
    // flip) fold into exactly one result row after the call row.
    let frames = vec![
        tool_call("c1", "read", serde_json::json!({"path": "a.md"})),
        status_update("c1", ToolCallStatus::InProgress),
        tool_result(
            "c1",
            serde_json::json!({"path": "a.md", "content": "l1\nl2"}),
        ),
        status_update("c1", ToolCallStatus::Completed),
    ];
    let lines = transcript_from_frames(&frames);
    assert_eq!(
        lines.len(),
        2,
        "one call row + one result row, got {lines:?}"
    );
    assert!(matches!(
        &lines[1],
        TranscriptLine::Tool { name, body, .. } if name == "result" && body == "Read 2 lines"
    ));
}

#[test]
fn test_status_emits_no_result() {
    // A call whose output has not landed shows the chip only — no
    // phantom "Read 0 lines" result row derived from a Null output.
    let frames = vec![
        tool_call("c1", "read", serde_json::json!({"path": "a.md"})),
        status_update("c1", ToolCallStatus::InProgress),
    ];
    let lines = transcript_from_frames(&frames);
    assert_eq!(lines.len(), 1, "call row only, got {lines:?}");
    assert!(matches!(
        &lines[0],
        TranscriptLine::Tool { name, .. } if name == "read"
    ));
}

#[test]
fn test_result_groups_under_call() {
    // Parallel calls whose results arrive after both calls: each result
    // renders directly under its own call, not at the update frame's
    // stream position (results used to pile up under the wrong call).
    let frames = vec![
        tool_call("c1", "read", serde_json::json!({"path": "a.md"})),
        tool_call("c2", "read", serde_json::json!({"path": "b.md"})),
        tool_result("c1", serde_json::json!({"path": "a.md", "content": "x"})),
        tool_result("c2", serde_json::json!({"path": "b.md", "content": "y\nz"})),
    ];
    let lines = transcript_from_frames(&frames);
    assert_eq!(lines.len(), 4);
    let ids: Vec<&str> = lines
        .iter()
        .map(|l| match l {
            TranscriptLine::Tool { call_id, .. } => call_id.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(ids, vec!["c1", "c1", "c2", "c2"], "result follows its call");
}

#[test]
fn test_todo_pair_dropped() {
    // The normal path skips both the todo_write call and result rows; the
    // checklist widget owns rendering.
    let frames = vec![
        tool_call(
            "c1",
            "todo_write",
            serde_json::json!({"todos": [{"content": "t", "status": "pending"}]}),
        ),
        tool_result(
            "c1",
            serde_json::json!({"todos": [], "old_todos": [], "total": 0}),
        ),
    ];
    let lines = transcript_from_frames(&frames);
    assert!(lines.is_empty(), "todo pair renders nothing, got {lines:?}");
}

#[test]
fn test_todo_result_orphan_dropped() {
    // A todo result whose call frame scrolled out of the rebuilt window has
    // no tools entry, so its name is unknown. Without the old_todos shape
    // guard it leaked as a raw {"todos":...} result row.
    let frames = vec![tool_result(
        "c1",
        serde_json::json!({"todos": [], "old_todos": [], "total": 0}),
    )];
    let lines = transcript_from_frames(&frames);
    assert!(
        lines.is_empty(),
        "orphan todo result must not leak, got {lines:?}"
    );
}

#[test]
fn test_non_todo_orphan_renders() {
    // The orphan shape guard targets todo_write only; an orphan result for
    // any other tool still surfaces its output.
    let frames = vec![tool_result("c1", serde_json::json!({"stdout": "hello"}))];
    let lines = transcript_from_frames(&frames);
    assert_eq!(
        lines.len(),
        1,
        "non-todo orphan still renders, got {lines:?}"
    );
    assert!(matches!(
        &lines[0],
        TranscriptLine::Tool { name, body, .. } if name == "result" && body == "hello"
    ));
}

#[test]
fn test_save_memory_label() {
    // save_memory collapses to a human label: the chip shows the key (never
    // the content field), and the result body is "stored <key>" — the raw
    // {"saved":...} JSON and the passed content both stay out of the
    // readable transcript.
    let frames = vec![
        tool_call(
            "c1",
            "save_memory",
            serde_json::json!({
                "key": "proj-status",
                "description": "build state",
                "source": "project",
                "content": "secret content must not leak"
            }),
        ),
        tool_result("c1", serde_json::json!({"saved": "proj-status"})),
    ];
    let lines = transcript_from_frames(&frames);
    assert_eq!(lines.len(), 2, "one chip + one result, got {lines:?}");
    assert!(
        matches!(
            &lines[0],
            TranscriptLine::Tool { name, invocation, .. }
                if name == "save_memory" && invocation == "proj-status"
        ),
        "chip must show the key only, got {:?}",
        lines[0]
    );
    assert!(
        matches!(
            &lines[1],
            TranscriptLine::Tool { name, body, .. }
                if name == "result" && body == "stored proj-status"
        ),
        "result must be the stored label, got {:?}",
        lines[1]
    );
    let joined = format!("{lines:?}");
    assert!(!joined.contains("secret content"), "content must not leak");
    assert!(!joined.contains("\"saved\""), "raw JSON must not leak");
}

#[test]
fn test_reused_id_keeps_body() {
    // Eager tool callers (qwen-class) sometimes reuse one call_id across
    // two distinct tool calls. Each result row must carry its OWN output —
    // a HashMap keyed by call_id collapses to the last insert, so every
    // result showed the last edit's body. FIFO consume (one update per
    // call) preserves each.
    let frames = vec![
        tool_call("c1", "bash", serde_json::json!({"command": "echo aaa"})),
        tool_result("c1", serde_json::json!({"stdout": "aaa"})),
        tool_call("c1", "bash", serde_json::json!({"command": "echo bbb"})),
        tool_result("c1", serde_json::json!({"stdout": "bbb"})),
    ];
    let lines = transcript_from_frames(&frames);
    let bodies: Vec<String> = lines
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::Tool { name, body, .. } if name == "result" => Some(body.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(bodies.len(), 2, "two result rows, got {bodies:?}");
    assert!(
        bodies[0].contains("aaa"),
        "first result carries its own output aaa, got {}",
        bodies[0]
    );
    assert!(
        !bodies[0].contains("bbb"),
        "first result must not show the later edit's body, got {}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("bbb"),
        "second result carries its own output bbb, got {}",
        bodies[1]
    );
}

#[test]
fn test_empty_agent_chunk_skipped() {
    let frames = vec![agent_msg("")];
    let lines = transcript_from_frames(&frames);
    assert!(lines.is_empty());
}

#[test]
fn test_extract_body_edit_diff() {
    let out = serde_json::json!({
        "path": "src/lib.rs",
        "diff": "@@ -1,2 +1,2 @@\n fn a() {\n-    1\n+    2\n }\n",
        "occurrences_replaced": 1,
        "bytes": 20,
    })
    .to_string();
    let body = extract_body(&out);
    // Summary "Added 1 line, removed 1 line", then the diff body.
    assert!(body.starts_with("Added 1 line, removed 1 line\n"));
    assert!(body.contains("-    1"));
    assert!(body.contains("+    2"));
}

#[test]
fn test_extract_body_bash_stdout() {
    let out = serde_json::json!({"stdout": "hello\nworld"}).to_string();
    assert_eq!(extract_body(&out), "hello\nworld");
}

#[test]
fn test_bash_body_no_dup() {
    // The bash result body must be the raw stdout, not the summary
    // (first line) prepended to the raw — that duplicates line 1 and
    // surfaces markup twice when the head is HTML or a code fence.
    let frames = vec![
        tool_call(
            "c1",
            "bash",
            serde_json::json!({"command": "cat README.md"}),
        ),
        tool_result(
            "c1",
            serde_json::json!({"stdout": "<div align=\"center\">\n\n```\nascii art"}),
        ),
    ];
    let lines = transcript_from_frames(&frames);
    let body = match &lines[1] {
        TranscriptLine::Tool { body, .. } => body.clone(),
        other => panic!("expected result row, got {other:?}"),
    };
    assert_eq!(
        body, "<div align=\"center\">\n\n```\nascii art",
        "bash body must not duplicate the first stdout line"
    );
}

#[test]
fn test_extract_body_read_content() {
    let out = serde_json::json!({"path": "p", "content": "line1\nline2"}).to_string();
    assert_eq!(extract_body(&out), "line1\nline2");
}

#[test]
fn test_extract_body_write_bytes() {
    let out = serde_json::json!({"path": "p", "bytes": 42}).to_string();
    assert_eq!(extract_body(&out), "wrote p (42 bytes)");
}

#[test]
fn test_extract_body_error() {
    let out = serde_json::json!({"error": "boom"}).to_string();
    assert_eq!(extract_body(&out), "error: boom");
}

#[test]
fn test_read_error_surfaces() {
    // A failed read (error field, no content) must surface the error in
    // the result row body, not be swallowed as "Read 0 lines".
    let frames = vec![
        tool_call("r1", "read", serde_json::json!({"path": "/secret"})),
        tool_result("r1", serde_json::json!({"error": "permission denied"})),
    ];
    let lines = transcript_from_frames(&frames);
    let result = lines
        .iter()
        .find_map(|l| match l {
            TranscriptLine::Tool { name, body, .. } if name == "result" => Some(body.clone()),
            _ => None,
        })
        .unwrap();
    assert!(result.contains("error"), "got: {result}");
    assert!(!result.contains("Read 0"), "swallowed error as: {result}");
}

#[test]
fn test_extract_body_plain_string() {
    // A non-JSON plain-string result (stub) is shown verbatim.
    assert_eq!(extract_body("just text"), "just text");
}

#[test]
fn test_count_lines_skips_headers() {
    let diff = "--- a\n+++ b\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n";
    assert_eq!(count_diff_lines(diff), (1, 1));
}

#[test]
fn test_output_has_diff_detects() {
    assert!(output_has_diff(
        r#"{"path":"a","diff":"@@ -1 +1 @@\n-x\n+y\n"}"#
    ));
    assert!(!output_has_diff(r#"{"stdout":"hi"}"#));
    assert!(!output_has_diff("not json"));
}

#[test]
fn test_diff_result_marked_diff() {
    // An Edit result (carries a diff) -> is_diff true; a Bash result -> false.
    let frames = vec![
        tool_result(
            "c1",
            serde_json::json!({
                "path": "a.rs",
                "diff": "@@ -1 +1 @@\n-old\n+new\n",
                "occurrences_replaced": 1,
                "bytes": 4,
            }),
        ),
        tool_result("c2", serde_json::json!({"stdout": "hi"})),
    ];
    let lines = transcript_from_frames(&frames);
    let edit = &lines[0];
    assert!(matches!(edit, TranscriptLine::Tool { is_diff: true, .. }));
    let (body, is_diff) = edit.result_body();
    assert!(is_diff);
    assert!(body.starts_with("Added 1 line, removed 1 line"));
    let bash = &lines[1];
    let (_, is_diff_bash) = bash.result_body();
    assert!(!is_diff_bash);
}

#[test]
fn test_thought_chunk_becomes_thinking() {
    let frames = vec![thought("pondering")];
    let lines = transcript_from_frames(&frames);
    assert!(matches!(
        lines[0],
        TranscriptLine::Thinking { ref text } if text == "pondering"
    ));
}

#[test]
fn test_turn_reasoning_last_user() {
    // Reasoning before the last user message is excluded; only the
    // current turn's thought chunks concatenate into the expand text.
    let frames = vec![
        thought("old turn"),
        user_msg("go"),
        thought("pondering "),
        thought("deeply"),
    ];
    assert_eq!(turn_reasoning(&frames).as_deref(), Some("pondering deeply"));
}

#[test]
fn test_turn_summary_follows_user() {
    let frames = vec![
        tool_call("c0", "bash", serde_json::Value::Null),
        user_msg("go"),
        tool_call("c1", "bash", serde_json::Value::Null),
        tool_call("c2", "grep", serde_json::Value::Null),
    ];
    assert_eq!(
        turn_tool_summary(&frames).as_deref(),
        Some("ran 2 tools (1 bash, 1 grep)")
    );
}

#[test]
fn test_turn_summary_no_tools() {
    let frames = vec![user_msg("go"), agent_msg("ok")];
    assert!(turn_tool_summary(&frames).is_none());
}
