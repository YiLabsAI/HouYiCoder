//! Wire-frame-to-transcript projection: rebuild the readable transcript lines
//! from the ordered session/update + acpx frame stream the driver accumulates,
//! plus the per-turn reasoning + tool-summary folds the ThoughtFor row
//! surfaces. Split from records.rs so each file stays under the size gate.

use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate, ToolCallStatus};

use crate::brief::{result_summary, tool_call_brief};
use crate::records::{ToolOutcome, TranscriptLine};

/// The transcript-snapshot seam (a loader backed by the durable log) lives
/// as a directory submodule here so its path is transcript::snapshot, not a
/// flat-prefix sibling of this file.
pub mod snapshot;
#[cfg(test)]
use crate::result_body::count_diff_lines;
use crate::result_body::{extract_body, output_has_diff, write_result_body};

/// One frame of the wire turn stream, preserved in arrival order so the
/// transcript rebuild keeps the time-ordered interleave of session/update
/// chunks and acpx/context/* audit notifications (a compaction checkpoint
/// lands between the tool calls that bracketed it, not at the tail). The
/// driver accumulates these as the server pushes them; the transcript is a
/// faithful projection of that ordered stream, never a stub.
#[derive(Debug, Clone)]
pub enum TranscriptFrame {
    /// An ACP session/update chunk (user / agent / thought message, tool call,
    /// tool-call update). The standard turn stream the base protocol carries.
    Session(SessionUpdate),
    /// An acpx/* extension notification — durable-context audit kinds the
    /// base session/update has no variant for (compaction boundary, summary,
    /// permission decision, meta user), or a token-level provider event.
    Acpx(AcpxNotification),
}

/// The text carried by a content chunk, when the chunk wraps a text block.
/// Non-text blocks (Image) have no flat text; an empty string degenerates the
/// line away so a multimodal chunk does not surface as an empty row.
pub fn chunk_text(chunk: &ContentChunk) -> String {
    match &chunk.content {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    }
}

/// Rebuild the transcript from the ordered wire frame stream the driver
/// accumulated. Each SessionUpdate maps to one TranscriptLine; the acpx
/// audit kinds the transcript surfaces (compaction, summary) become System
/// lines; the meta-user nudge + permission-decision audit are dropped
/// (control-only). Tool-call outcomes are resolved in a first pass from the
/// matching ToolCallUpdate so the call chip colors by outcome.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub fn transcript_from_frames(frames: &[TranscriptFrame]) -> Vec<TranscriptLine> {
    // First pass: resolve each tool call's outcome + output from its matching
    // ToolCallUpdate (by tool_call_id) so the call chip colors by outcome and
    // the result row carries the precomputed body. Also record the tool name
    // + raw_input from the ToolCall so the result row's brief is correct.
    use std::collections::HashMap;
    // Tool-call updates are kept in an ordered Vec and consumed FIFO per
    // call_id, not a last-write-wins HashMap. Eager tool callers reuse one
    // call_id across distinct calls; a HashMap would collapse them to the last
    // insert and every result row would show the same body. FIFO consume
    // pairs each call with its own matching update. The tools map stays a
    // HashMap: the call row reads the title + input from the ToolCall frame
    // itself, and tools only names the tool for an orphan result (no call
    // frame in the stream), where first-write is fine.
    let mut updates: Vec<(String, Option<ToolOutcome>, Option<serde_json::Value>)> = Vec::new();
    let mut tools: HashMap<String, (String, Option<serde_json::Value>)> = HashMap::new();
    for f in frames {
        match f {
            TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) => {
                tools.insert(
                    tc.tool_call_id.0.clone(),
                    (tc.title.clone(), tc.raw_input.clone()),
                );
            }
            TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) => {
                let id = upd.tool_call_id.0.clone();
                if let Some(out) = &upd.fields.raw_output {
                    updates.push((id, Some(ToolOutcome::from_output(out)), Some(out.clone())));
                } else if let Some(status) = upd.fields.status {
                    let oc = match status {
                        ToolCallStatus::Failed => ToolOutcome::Error,
                        ToolCallStatus::Completed => ToolOutcome::Success,
                        _ => ToolOutcome::Running,
                    };
                    updates.push((id, Some(oc), None));
                }
            }
            _ => {}
        }
    }
    // FIFO-consume the first update whose id matches, removing it so the next
    // call with the same id pairs with its own update (not the last insert).
    // Correctness relies on a call_id uniqueness invariant established at the
    // provider boundary (unique_id_gen in openai_compat.rs mints empty and
    // duplicate-within-response ids before any frame is built): with unique ids
    // each id has exactly one call and one update, so FIFO-by-arrival
    // degenerates to identity pairing regardless of completion order. If a
    // duplicate id ever reaches here, the earlier call silently steals the
    // first-arrived result for that id (pending_approvals and apply_decisions
    // in agent/mod.rs mis-route the same invariant the same way).
    fn take_update(
        updates: &mut Vec<(String, Option<ToolOutcome>, Option<serde_json::Value>)>,
        id: &str,
    ) -> Option<(Option<ToolOutcome>, Option<serde_json::Value>)> {
        let pos = updates.iter().position(|(cid, _, _)| cid == id)?;
        let (_, oc, out) = updates.remove(pos);
        Some((oc, out))
    }
    let result_line = |id: &str,
                       tool_name: &str,
                       output: &serde_json::Value,
                       call_input: Option<&serde_json::Value>| {
        let out_str = output.to_string();
        // The Read tool result shows only a one-line summary (Read N
        // lines): the file content
        // goes to the model via the tool-result block, never the
        // transcript. Dumping content flooded the transcript and
        // enabled the duplication bug (bug-log #27). The content stays
        // in the frame log for a future expand-on-demand improvement; the
        // body is the summary alone. Bash shows raw stdout directly — its
        // summary is the first stdout line, which the raw body already
        // starts with, so prepending it duplicates line 1. Other tools
        // (grep matches, edit diffs) keep summary + raw (their summary is
        // a count/label, not a line of the raw body).
        let raw = extract_body(&out_str);
        let body = if tool_name == "read" {
            // A failed read (permission denied, not found, sandbox reject)
            // carries an "error" field, no "content" — result_summary would
            // count 0 lines and swallow the real cause as "Read 0 lines".
            // extract_body already formats "error: <msg>"; use it on error.
            if output.get("error").is_some() {
                raw
            } else {
                result_summary(tool_name, output).unwrap_or_default()
            }
        } else if tool_name == "bash" {
            raw
        } else if tool_name == "write" {
            // "Wrote N lines to {path}" chip + the full written content.
            // The content is pulled from the call's input (the model sent
            // it to write); the result stays path-only for the model.
            // Folding (first-N visible + overflow tail + expand toggle)
            // is the render layer's job via tool_rows, not baked into the
            // body. Without call_input (a late-arriving result whose call
            // frame already passed) the chip alone surfaces.
            write_result_body(output, call_input)
        } else {
            match result_summary(tool_name, output) {
                Some(s) if raw.is_empty() => s,
                Some(s) => format!("{s}\n{raw}"),
                None => raw,
            }
        };
        TranscriptLine::Tool {
            name: "result".to_string(),
            tool: tool_name.to_string(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::from_output(output),
            call_id: id.to_string(),
            body,
            is_diff: output_has_diff(&out_str),
        }
    };
    let mut out = Vec::with_capacity(frames.len());
    // Late-arriving results (their ToolCall frame already passed when the
    // matching ToolCallUpdate lands). The main loop defers them; a reposition
    // pass after the loop inserts each right after its call row so a result is
    // never detached from its call or interleaved behind a thought. FIFO by
    // arrival order so the Nth late result for a reused call_id pairs with the
    // Nth matching call (matches take_update's FIFO consume).
    let mut late_results: Vec<(String, String, serde_json::Value)> = Vec::new();
    for f in frames {
        match f {
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(chunk)) => {
                out.push(TranscriptLine::User(chunk_text(chunk)));
            }
            TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(chunk)) => {
                let text = chunk_text(chunk);
                if !text.is_empty() {
                    out.push(TranscriptLine::Agent(text));
                }
            }
            TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(chunk)) => {
                out.push(TranscriptLine::Thinking {
                    text: chunk_text(chunk),
                });
            }
            TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) => {
                let id = &tc.tool_call_id.0;
                // Consume this call's own matching update (FIFO). When the
                // result has not landed yet, the update is absent and the call
                // row colors Running; when it has, the call row colors by
                // outcome and the result row carries the precomputed body.
                let upd = take_update(&mut updates, id);
                // todo_write renders only via the checklist widget (todo_view
                // parses the call's input from the frame log); the transcript
                // skips both its call row and result row so the tool does not
                // double-render as a chip alongside the widget. The frame
                // stays in the log for the widget + the verdict cursor.
                if tc.title == "todo_write" {
                    continue;
                }
                // The call row (skipped for the transparent HITL question
                // tool — its answer row below still renders).
                if tc.title != "AskUserQuestion" {
                    let outcome = upd
                        .as_ref()
                        .and_then(|(oc, _)| *oc)
                        .unwrap_or(ToolOutcome::Running);
                    let input = tc
                        .raw_input
                        .as_ref()
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    out.push(TranscriptLine::Tool {
                        name: crate::brief::tool_user_facing_name(&tc.title, &input).to_string(),
                        tool: tc.title.clone(),
                        status: tool_call_brief(&tc.title, &input),
                        invocation: houyicoder_protocol::tool::tool_invocation(&tc.title, &input),
                        outcome,
                        call_id: id.clone(),
                        body: String::new(),
                        is_diff: false,
                    });
                }
                // The single result row, grouped under its call. Only when a
                // real output landed — no output means the chip color is the
                // whole story, not a phantom result row.
                if let Some((_, Some(output))) = upd {
                    out.push(result_line(id, &tc.title, &output, tc.raw_input.as_ref()));
                }
            }
            TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) => {
                // A late-arriving result (its ToolCall frame already passed,
                // so take_update at the call found nothing then). Updates
                // whose ToolCall was present AND already consumed return None
                // here and skip. Do NOT push inline at the arrival position —
                // that detaches the result from its call and lets a thought
                // interleave between them. Defer; a reposition pass attaches
                // each late result right after its call row.
                let id = &upd.tool_call_id.0;
                if let Some((_, Some(output))) = take_update(&mut updates, id) {
                    let (tool_name, _) = tools.get(id).cloned().unwrap_or_default();
                    // todo_write's result is boilerplate ("Todos modified");
                    // the call row is skipped above, so a late result would
                    // orphan. Drop it — the widget owns todo_write rendering.
                    if tool_name != "todo_write" {
                        late_results.push((id.clone(), tool_name, output));
                    }
                }
            }
            TranscriptFrame::Acpx(n) => match n.method {
                AcpxMethod::ContextCompactionBoundary => {
                    out.push(TranscriptLine::System("compaction checkpoint".to_string()));
                }
                AcpxMethod::ContextSummary => {
                    let text = n
                        .params
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(TranscriptLine::System(format!("summary: {text}")));
                }
                // Audit-only: the meta-user nudge is a control message the
                // runner injects (never authored by the human); the verdict
                // is already visible via the approval card. Both stay out of
                // the readable transcript.
                AcpxMethod::ContextMetaUser | AcpxMethod::ContextPermissionDecision => {}
                _ => {}
            },
            // A future SessionUpdate variant the transcript does not render
            // yet (Plan, SessionInfoUpdate, ...) is ignored so the rebuild
            // never fails on a shape the frontend does not model.
            _ => {}
        }
    }
    // Reposition pass: attach each late result right after its matching call
    // row so a result that arrived after a thought pulls back to its call
    // (preserving call+result adjacency + input order). A late result whose
    // call row is absent (compacted) falls through to the tail. Forward search
    // for the first call row with the matching id; the harness ships one
    // durable update per call, so at most one late result per id lands here
    // (an orphan whose call was compacted out), and the first match is the
    // right one. Skip past any result rows already placed for THIS call_id so
    // multiple late results for one call stack in arrival order without
    // detaching an edit's diff from its call.
    for (id, tool_name, output) in late_results {
        let mut insert_at: Option<usize> = None;
        for (i, line) in out.iter().enumerate() {
            if let TranscriptLine::Tool { name, call_id, .. } = line
                && name != "result"
                && call_id == &id
            {
                let mut j = i + 1;
                while j < out.len()
                    && matches!(
                        &out[j],
                        TranscriptLine::Tool { name: nm, call_id: cid, .. }
                        if nm == "result" && cid == &id
                    )
                {
                    j += 1;
                }
                insert_at = Some(j);
                break;
            }
        }
        let row = result_line(&id, &tool_name, &output, None);
        match insert_at {
            Some(pos) => out.insert(pos, row),
            None => out.push(row),
        }
    }
    out
}

/// Extract the current turn's reasoning: scan from the last user message
/// chunk onward so a Ctrl+O expand shows only this turn's chain of thought,
/// not a concatenation of every prior turn's reasoning.
pub fn turn_reasoning(frames: &[TranscriptFrame]) -> Option<String> {
    let last_user = frames.iter().rposition(|f| {
        matches!(
            f,
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
        )
    });
    let start = last_user.map(|i| i + 1).unwrap_or(0);
    let mut r = String::new();
    for f in &frames[start..] {
        if let TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(chunk)) = f {
            r.push_str(&chunk_text(chunk));
        }
    }
    if r.is_empty() { None } else { Some(r) }
}

/// A one-line summary of the tools the current turn invoked, in the shape
/// the folded ThoughtFor row surfaces ("ran 3 tools (2 bash, 1 grep)").
/// Scans from the last user message chunk onward so only this turn's tool
/// calls land in the summary. Returns None when the turn ran no tools.
pub fn turn_tool_summary(frames: &[TranscriptFrame]) -> Option<String> {
    let last_user = frames.iter().rposition(|f| {
        matches!(
            f,
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
        )
    });
    let start = last_user.map(|i| i + 1).unwrap_or(0);
    let mut counts: Vec<(String, u32)> = Vec::new();
    let mut total = 0u32;
    for f in &frames[start..] {
        if let TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) = f {
            if tc.title == "AskUserQuestion" {
                continue;
            }
            if let Some(slot) = counts.iter_mut().find(|(t, _)| t == &tc.title) {
                slot.1 += 1;
            } else {
                counts.push((tc.title.clone(), 1));
            }
            total += 1;
        }
    }
    if total == 0 {
        return None;
    }
    counts.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    let parts: Vec<String> = counts.iter().map(|(t, c)| format!("{c} {t}")).collect();
    let noun = if total == 1 { "tool" } else { "tools" };
    Some(format!("ran {total} {noun} ({})", parts.join(", ")))
}

#[cfg(test)]
mod tests {
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
    fn test_acpx_compaction_summary_surface() {
        // Compaction boundary + summary ride the acpx stream; both become
        // System lines at their ordered positions. The meta-user nudge and
        // permission-decision audit do not surface.
        let frames = vec![
            user_msg("hi"),
            TranscriptFrame::Acpx(AcpxNotification::new(
                AcpxMethod::ContextCompactionBoundary,
                serde_json::json!({ "checkpoint": "01J00000000000000000000000" }),
            )),
            TranscriptFrame::Acpx(AcpxNotification::new(
                AcpxMethod::ContextSummary,
                serde_json::json!({ "text": "prior turn condensed" }),
            )),
            TranscriptFrame::Acpx(AcpxNotification::new(
                AcpxMethod::ContextMetaUser,
                serde_json::json!({ "text": "nudge" }),
            )),
        ];
        let lines = transcript_from_frames(&frames);
        // User + compaction + summary; meta-user dropped.
        assert_eq!(lines.len(), 3);
        assert!(matches!(lines[0], TranscriptLine::User(_)));
        assert!(matches!(
            lines[1],
            TranscriptLine::System(ref s) if s == "compaction checkpoint"
        ));
        assert!(matches!(
            lines[2],
            TranscriptLine::System(ref s) if s == "summary: prior turn condensed"
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
}

#[cfg(test)]
#[path = "transcript_reattach_tests.rs"]
mod reattach_tests;
