use super::*;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::Value;

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

#[test]
fn test_frames_map_to_transcript() {
    let frames = vec![
        user_msg("hi"),
        agent_msg("hello back"),
        tool_call("c1", "bash", serde_json::json!({"command": "ls"})),
        tool_result("c1", serde_json::json!({"stdout": "file.txt"})),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    // Four frame lines plus the turn's summary row, which the projection
    // derives from the same frames and places where the turn ended.
    assert_eq!(lines.len(), 5);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    assert!(
        lines.is_empty(),
        "orphan todo result must not leak, got {lines:?}"
    );
}

#[test]
fn test_non_todo_orphan_dropped() {
    // A non-todo orphan (its call frame compacted out of the window) is
    // dropped, not stranded at the tail. The body stays in the frame log for
    // search; the transcript only renders a result beside its call.
    let frames = vec![tool_result("c1", serde_json::json!({"stdout": "hello"}))];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    assert!(
        lines.is_empty(),
        "a non-todo orphan must not strand at the tail: {lines:?}"
    );
}

#[test]
fn test_save_memory_label() {
    // save_memory collapses to a human label: the chip shows the key (never
    // the content field), and the result body is the outcome label — the raw
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
        tool_result(
            "c1",
            serde_json::json!({"saved": "proj-status", "outcome": "created"}),
        ),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
                if name == "result" && body == "created proj-status"
        ),
        "result must be the created label, got {:?}",
        lines[1]
    );
    let joined = format!("{lines:?}");
    assert!(!joined.contains("secret content"), "content must not leak");
    assert!(!joined.contains("\"saved\""), "raw JSON must not leak");
}

#[test]
fn test_search_memory_label() {
    // search_memory collapses to a match count: the chip shows the query and
    // the result body is "found N". The matches JSON is a machine-readable
    // list, not a readable body, so it stays out of the transcript — the same
    // contract the other memory tools hold.
    let frames = vec![
        tool_call(
            "c1",
            "search_memory",
            serde_json::json!({ "query": "how the deploy gate works" }),
        ),
        tool_result(
            "c1",
            serde_json::json!({"matches": [
                {"key": "deploy-gate", "description": "red until review"},
                {"key": "tea-order", "description": "how the team orders tea"}
            ]}),
        ),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    assert_eq!(lines.len(), 2, "one chip + one result, got {lines:?}");
    assert!(
        matches!(
            &lines[0],
            TranscriptLine::Tool { name, invocation, .. }
                if name == "search_memory" && invocation == "how the deploy gate works"
        ),
        "chip must show the query only, got {:?}",
        lines[0]
    );
    assert!(
        matches!(
            &lines[1],
            TranscriptLine::Tool { name, body, .. }
                if name == "result" && body == "found 2"
        ),
        "result must be the match count, got {:?}",
        lines[1]
    );
    let joined = format!("{lines:?}");
    assert!(!joined.contains("tea-order"), "match keys must not leak");
    assert!(!joined.contains("\"matches\""), "raw JSON must not leak");
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
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
        tool_call("c1", "edit", serde_json::json!({"path": "a.rs"})),
        tool_result(
            "c1",
            serde_json::json!({
                "path": "a.rs",
                "diff": "@@ -1 +1 @@\n-old\n+new\n",
                "occurrences_replaced": 1,
                "bytes": 4,
            }),
        ),
        tool_call("c2", "bash", serde_json::json!({"command": "echo hi"})),
        tool_result("c2", serde_json::json!({"stdout": "hi"})),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let edit_result = lines
        .iter()
        .find(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name == "result" && call_id == "c1"))
        .expect("edit result row");
    let (body, is_diff) = edit_result.result_body();
    assert!(is_diff, "edit result is a diff");
    assert!(
        body.starts_with("Added 1 line, removed 1 line"),
        "edit result body: {body}"
    );
    let bash_result = lines
        .iter()
        .find(|l| matches!(l, TranscriptLine::Tool { name, call_id, .. } if name == "result" && call_id == "c2"))
        .expect("bash result row");
    let (_, is_diff_bash) = bash_result.result_body();
    assert!(!is_diff_bash, "bash result is not a diff");
}

#[test]
fn test_thought_chunk_becomes_thinking() {
    let frames = vec![thought("pondering")];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    assert!(matches!(
        lines[0],
        TranscriptLine::Thinking { ref text } if text == "pondering"
    ));
}

fn run_completed(ms: Option<u64>) -> TranscriptFrame {
    TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextRunCompleted,
        serde_json::json!({ "ms": ms }),
    ))
}

/// A message the user sent while the turn was running, as the projection
/// writes it: the message chunk followed by the mark that says the turn kept
/// going past it.
fn mid_turn_msg(text: &str) -> [TranscriptFrame; 2] {
    [
        user_msg(text),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextMidTurnInput,
            serde_json::json!({}),
        )),
    ]
}

/// A background child's result handed to the running turn, written the same
/// way a queued message is.
fn child_completed_msg(text: &str) -> [TranscriptFrame; 2] {
    [
        user_msg(text),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextChildCompleted,
            serde_json::json!({}),
        )),
    ]
}

/// The notice a regenerate leaves behind after a cancel or a restart, written
/// the way the projection writes it: the notice chunk followed by the mark
/// that says it belongs to the turn it interrupts.
fn interrupted_notice(text: &str) -> [TranscriptFrame; 2] {
    [
        user_msg(text),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextTurnInterrupted,
            serde_json::json!({}),
        )),
    ]
}

/// The parts of a turn's summary row a test reads.
struct RowFacts {
    ms: Option<u64>,
    reasoning: Option<String>,
    tool_summary: Option<String>,
    turn_id: String,
}

fn thought_row(lines: &[TranscriptLine]) -> Option<RowFacts> {
    lines.iter().find_map(|l| match l {
        TranscriptLine::ThoughtFor {
            ms,
            reasoning,
            tool_summary,
            turn_id,
        } => Some(RowFacts {
            ms: *ms,
            reasoning: reasoning.clone(),
            tool_summary: tool_summary.clone(),
            turn_id: turn_id.clone(),
        }),
        _ => None,
    })
}

#[test]
fn test_turn_row_folds_reasoning() {
    // Each turn's reasoning folds into that turn's row. Reasoning ahead of
    // the turn must not join it, and the row lands after the answer it
    // summarizes because that is where the turn ended.
    let frames = vec![
        thought("old turn"),
        user_msg("go"),
        thought("pondering "),
        thought("deeply"),
        agent_msg("done"),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let row = thought_row(&lines).expect("a row for the turn");
    assert_eq!(row.reasoning.as_deref(), Some("pondering deeply"));
    assert_eq!(row.ms, None, "a log with no record claims no duration");
    assert_eq!(row.turn_id, "f4", "named by the frame that ended the turn");
    assert!(matches!(
        lines.last(),
        Some(TranscriptLine::ThoughtFor { .. })
    ));
}

#[test]
fn test_turn_row_counts_tools() {
    // The summary counts the turn's own calls, grouped by tool with the
    // most-used first. A call from the turn before stays out.
    let frames = vec![
        tool_call("c0", "bash", Value::Null),
        user_msg("go"),
        tool_call("c1", "bash", Value::Null),
        tool_call("c2", "grep", Value::Null),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let row = thought_row(&lines).expect("a row for the turn");
    assert_eq!(
        row.tool_summary.as_deref(),
        Some("ran 2 tools (1 bash, 1 grep)")
    );
}

#[test]
fn test_turn_row_one_kind() {
    // One kind of tool reads as its own count: the breakdown paren would only
    // repeat the total it sits behind, so "ran 3 bash", never "ran 3 tools
    // (3 bash)".
    let frames = vec![
        user_msg("go"),
        tool_call("c1", "bash", Value::Null),
        tool_call("c2", "bash", Value::Null),
        tool_call("c3", "bash", Value::Null),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let row = thought_row(&lines).expect("a row for the turn");
    assert_eq!(row.tool_summary.as_deref(), Some("ran 3 bash"));
}

#[test]
fn test_turn_row_two_kinds() {
    // Two kinds keep the breakdown even when one kind dominates: the count of
    // the other kind has nowhere else to show.
    let frames = vec![
        user_msg("go"),
        tool_call("c1", "bash", Value::Null),
        tool_call("c2", "bash", Value::Null),
        tool_call("c3", "read", Value::Null),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let row = thought_row(&lines).expect("a row for the turn");
    assert_eq!(
        row.tool_summary.as_deref(),
        Some("ran 3 tools (2 bash, 1 read)")
    );
}

#[test]
fn test_plain_reply_no_row() {
    // A reply with nothing to expand renders no row: an affordance that
    // opens onto nothing reads as broken.
    let frames = vec![user_msg("go"), agent_msg("ok")];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    assert!(thought_row(&lines).is_none(), "got {lines:?}");
    assert_eq!(lines.len(), 2);
}

#[test]
fn test_row_uses_recorded_ms() {
    // The record of a finished run names the duration that run took, so a
    // turn rebuilt from the log shows the duration the live turn showed.
    let frames = vec![
        user_msg("go"),
        thought("weighing"),
        agent_msg("answer"),
        run_completed(Some(7_000)),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let row = thought_row(&lines).expect("a row for the turn");
    assert_eq!(row.ms, Some(7_000));
    assert_eq!(
        row.turn_id, "f3",
        "the record is the frame that ended the turn"
    );
    assert!(matches!(lines[3], TranscriptLine::ThoughtFor { .. }));
}

/// Both ends spell the duration key out by hand, so this decodes the bytes a
/// peer sends rather than a fixture built with the reader's own spelling. A
/// drift in either one loses every turn's duration and fails nothing else.
#[test]
fn test_row_reads_duration_key() {
    let raw = r#"{"method":"acpx/context/run_completed","params":{"ms":620}}"#;
    let n: AcpxNotification = serde_json::from_str(raw).expect("the peer's frame decodes");
    let frames = vec![
        user_msg("go"),
        thought("weighing"),
        TranscriptFrame::Acpx(n),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    assert_eq!(
        thought_row(&lines).expect("a row for the turn").ms,
        Some(620)
    );
}

#[test]
fn test_record_unknown_duration_closes() {
    // A turn that ended without a measured loop, an abort while paused on an
    // approval, still records its end. The record closes the turn and leaves
    // the duration unknown, so the row reads the same as one for a turn whose
    // loop ran; the turn after it starts fresh rather than inheriting.
    let frames = vec![
        user_msg("go"),
        thought("weighing"),
        agent_msg("answer"),
        run_completed(None),
        user_msg("next"),
        thought("again"),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let rows = lines
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
        .count();
    assert_eq!(rows, 2, "one row per turn: {lines:?}");
    let row = thought_row(&lines).expect("a row");
    assert_eq!(row.ms, None, "an unmeasured loop claims no duration");
    assert_eq!(row.reasoning.as_deref(), Some("weighing"));
    assert_eq!(row.turn_id, "f3");
}

#[test]
fn test_open_turn_no_row() {
    // A turn the caller reports as still running gets no row yet: one closed
    // at the window end would describe a half-accumulated turn as a whole.
    let frames = vec![user_msg("go"), thought("thinking"), agent_msg("part")];
    let lines = transcript_from_frames(&frames, 0..frames.len(), true);
    assert!(thought_row(&lines).is_none(), "got {lines:?}");
    assert_eq!(lines.len(), 3);
}

#[test]
fn test_turn_name_uses_log() {
    // The name is the turn's position in the log, not its position in the
    // slice given to the projection, so a window that slides keeps naming
    // the same turn and the expand state stays with its row.
    let mut frames: Vec<TranscriptFrame> = (0..100).map(|_| agent_msg("")).collect();
    frames.push(user_msg("go"));
    frames.push(thought("thinking"));
    let lines = transcript_from_frames(&frames, 100..frames.len(), false);
    let row = thought_row(&lines).expect("a row for the turn");
    assert_eq!(row.turn_id, "f101");
}

#[test]
fn test_record_spans_mid_turn() {
    // A message that lands inside a turn, a queued interjection, does not end
    // it: the mark beside the message says the turn kept running. The row
    // therefore spans the whole turn rather than splitting where the message
    // arrived.
    let mut frames = vec![user_msg("go"), thought("first half ")];
    frames.extend(mid_turn_msg("queued note"));
    frames.push(tool_call("c1", "bash", Value::Null));
    frames.push(thought("second half"));
    frames.push(run_completed(Some(3_000)));
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let rows = lines
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
        .count();
    assert_eq!(rows, 1, "one row for the one turn: {lines:?}");
    let row = thought_row(&lines).expect("a row");
    assert_eq!(row.ms, Some(3_000));
    assert_eq!(row.reasoning.as_deref(), Some("first half second half"));
    assert_eq!(row.tool_summary.as_deref(), Some("ran 1 bash"));
    assert_eq!(row.turn_id, "f6");
}

#[test]
fn test_frontend_row_between_mark() {
    // A notice raised while a queued message was in hand sits between that
    // message and the mark saying the running turn absorbed it. The message
    // still belongs to that turn, so the turn keeps the facts gathered before
    // it and ends once, at the record: reading the mark from the frame
    // immediately beside the message would end the turn there and leave the
    // facts gathered before it on a row of their own.
    let mut frames = vec![user_msg("go"), thought("first half ")];
    frames.push(user_msg("queued note"));
    frames.push(TranscriptFrame::Frontend(FrontendRow::System(
        "model set to haiku".into(),
    )));
    frames.push(TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextMidTurnInput,
        serde_json::json!({}),
    )));
    frames.push(thought("second half"));
    frames.push(run_completed(Some(3_000)));
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let rows = lines
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
        .count();
    assert_eq!(rows, 1, "one row for the one turn: {lines:?}");
    let row = thought_row(&lines).expect("a row");
    assert_eq!(row.reasoning.as_deref(), Some("first half second half"));
    assert_eq!(row.turn_id, "f6");
}

#[test]
fn test_child_notice_no_split() {
    // A background child's result is handed to the running turn the same way
    // a queued message is, so it folds into that turn rather than ending it.
    let mut frames = vec![user_msg("go"), thought("before ")];
    frames.extend(child_completed_msg("child found it"));
    frames.push(thought("after"));
    frames.push(run_completed(Some(2_000)));
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let rows = lines
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
        .count();
    assert_eq!(rows, 1, "one row for the one turn: {lines:?}");
    let row = thought_row(&lines).expect("a row");
    assert_eq!(row.reasoning.as_deref(), Some("before after"));
    assert_eq!(row.ms, Some(2_000));
}

#[test]
fn test_interrupt_notice_keeps_turn() {
    // A cancel inside a run regenerates in the same user turn, and so does a
    // restart. The notice it leaves is an event of that turn, so the turn's
    // work folds into one row: read as a message that opens a turn, the notice
    // would end the running turn and hand the record to a second row.
    let mut frames = vec![user_msg("go"), thought("before ")];
    frames.extend(interrupted_notice(
        "previous turn was interrupted, regenerated",
    ));
    frames.push(thought("after"));
    frames.push(run_completed(Some(4_000)));
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let rows = lines
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
        .count();
    assert_eq!(rows, 1, "one row for the one turn: {lines:?}");
    let row = thought_row(&lines).expect("a row");
    assert_eq!(row.reasoning.as_deref(), Some("before after"));
    assert_eq!(row.ms, Some(4_000), "the run's record closes that row");
}

#[test]
fn test_window_starts_inside_turn() {
    // A window whose oldest frames fall inside a turn still folds that turn:
    // the message that opened it lies behind the window, and the record that
    // ended it lies inside, so the row would otherwise be written with no
    // turn open and vanish. The window names the row by where the turn ended
    // in the log, which is a position the window carries.
    let mut frames = vec![user_msg("go"), thought("first half ")];
    frames.extend(mid_turn_msg("queued note"));
    frames.push(thought("second half"));
    frames.push(run_completed(Some(2_000)));
    let lines = transcript_from_frames(&frames, 4..frames.len(), false);
    let rows = lines
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
        .count();
    assert_eq!(rows, 1, "the turn the window cuts into keeps its row");
    let row = thought_row(&lines).expect("a row");
    assert_eq!(
        row.reasoning.as_deref(),
        Some("first half second half"),
        "the row summarizes the turn, not the part of it this window holds"
    );
    assert_eq!(
        row.ms,
        Some(2_000),
        "the record inside the window closed it"
    );
    assert_eq!(row.turn_id, "f5");
}

#[test]
fn test_window_short_of_record() {
    // A turn's row is written where the turn ended, so a window that stops
    // before the record carries no end and no row: the row appears when the
    // view reaches the record, not before. The whole log renders it.
    let mut frames = vec![user_msg("go"), thought("half ")];
    frames.extend(mid_turn_msg("queued note"));
    frames.push(thought("rest"));
    frames.push(run_completed(Some(5_000)));
    let windowed = transcript_from_frames(&frames, 0..5, false);
    assert!(
        thought_row(&windowed).is_none(),
        "no row while the window stops short of the turn's end: {windowed:?}"
    );
    let whole = transcript_from_frames(&frames, 0..frames.len(), false);
    let row = thought_row(&whole).expect("the whole log carries the row");
    assert_eq!(row.reasoning.as_deref(), Some("half rest"));
    assert_eq!(row.ms, Some(5_000));
}

#[test]
fn test_window_record_only() {
    // The window's oldest and only frame is the record that closed the turn, so
    // every fact the row summarizes lies ahead of the window. The row is still
    // the turn's row: a view that reaches the turn's end renders what the whole
    // log renders, rather than dropping it for having nothing in view.
    let mut frames = vec![user_msg("go"), thought("half ")];
    frames.push(tool_call("c1", "bash", Value::Null));
    frames.push(run_completed(Some(5_000)));
    let lines = transcript_from_frames(&frames, 3..4, false);
    let row = thought_row(&lines).expect("a row for the turn the record ends");
    assert_eq!(row.reasoning.as_deref(), Some("half "));
    assert_eq!(row.tool_summary.as_deref(), Some("ran 1 bash"));
    assert_eq!(row.ms, Some(5_000));
    assert_eq!(row.turn_id, "f3");
}

#[test]
fn test_upgraded_log_rows() {
    // A session started before the record existed and continued on a build
    // that writes one: the turns of the old era end at the next prompt, as
    // they always did, and the recorded turn ends at its record. Every prompt
    // keeps its own row.
    let frames = vec![
        user_msg("one"),
        thought("a"),
        user_msg("two"),
        thought("b"),
        user_msg("three"),
        thought("c"),
        run_completed(Some(9_000)),
        user_msg("four"),
        thought("d"),
        run_completed(Some(1_000)),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let facts: Vec<(Option<u64>, String, String)> = lines
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::ThoughtFor {
                ms,
                reasoning,
                turn_id,
                ..
            } => Some((*ms, reasoning.clone().unwrap_or_default(), turn_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        facts,
        vec![
            (None, "a".to_string(), "f1".to_string()),
            (None, "b".to_string(), "f3".to_string()),
            (Some(9_000), "c".to_string(), "f6".to_string()),
            (Some(1_000), "d".to_string(), "f9".to_string()),
        ],
        "one row per turn, each with its own reasoning: {lines:?}"
    );
}

#[test]
fn test_interrupted_turn_after_input() {
    // A turn carrying a queued message is interrupted: the interruption
    // notice is a message of its own, so it ends the turn and the partial
    // reasoning lands in that turn's row rather than splitting at the queued
    // message or running into the regenerated turn.
    let mut frames = vec![user_msg("go"), thought("first ")];
    frames.extend(mid_turn_msg("queued note"));
    frames.push(thought("second"));
    frames.push(user_msg("previous turn was interrupted, regenerated"));
    frames.push(user_msg("next"));
    frames.push(thought("third"));
    frames.push(run_completed(None));
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let facts: Vec<(Option<u64>, String)> = lines
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::ThoughtFor { ms, reasoning, .. } => {
                Some((*ms, reasoning.clone().unwrap_or_default()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        facts,
        vec![
            (None, "first second".to_string()),
            (None, "third".to_string()),
        ],
        "the interrupted turn keeps its partial summary: {lines:?}"
    );
}

#[test]
fn test_old_log_fallback_rows() {
    // A log written before the record existed ends its turns at the next
    // user message instead, so those turns keep a row each, without a
    // duration rather than with a fabricated one.
    let frames = vec![
        user_msg("first"),
        thought("one"),
        agent_msg("a"),
        user_msg("second"),
        thought("two"),
        agent_msg("b"),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let ms: Vec<Option<u64>> = lines
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::ThoughtFor { ms, .. } => Some(*ms),
            _ => None,
        })
        .collect();
    assert_eq!(ms, vec![None, None], "one row per turn: {lines:?}");
}
