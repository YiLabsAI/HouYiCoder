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
use crate::result_body::{
    command_is_silent_success, extract_body, output_has_diff, write_result_body,
};

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

/// Convert a fetched child frame to the live-frame shape the projection
/// reads, so child rows render through the same pipeline as the parent flow.
impl From<houyicoder_protocol::envelope::ChildTranscriptFrame> for TranscriptFrame {
    fn from(frame: houyicoder_protocol::envelope::ChildTranscriptFrame) -> Self {
        use houyicoder_protocol::envelope::ChildTranscriptFrame as C;
        match frame {
            C::Session(u) => TranscriptFrame::Session(u),
            C::Acpx(n) => TranscriptFrame::Acpx(n),
        }
    }
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
                    // Semantic error judgment needs the tool name + call input
                    // (grep/diff exit 1 is not an error). The tools map is
                    // populated by the ToolCall frame, which arrives before
                    // its update, so the entry is present here.
                    let (tool_name, call_input) = tools.get(&id).cloned().unwrap_or_default();
                    let outcome = ToolOutcome::from_output_with(
                        out,
                        &tool_name,
                        call_input.as_ref().unwrap_or(&serde_json::Value::Null),
                    );
                    updates.push((id, Some(outcome), Some(out.clone())));
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
            // A silent command (mv, cp, rm, mkdir, chmod, touch, cd, ...)
            // produces no output on success — that IS the success signal.
            // An empty body would render a bare "(no output)" placeholder,
            // which reads as "something went wrong". A "done" label tells
            // the user the command completed, which is what they need to
            // see for a command whose output is silence by design.
            if raw.is_empty() && command_is_silent_success(call_input, output) {
                "done".to_string()
            } else {
                raw
            }
        } else if tool_name == "save_memory"
            || tool_name == "delete_memory"
            || tool_name == "promote_memory"
            || tool_name == "demote_memory"
            || tool_name == "show_memory"
        {
            // The result is a machine-readable JSON naming the memory key
            // (and for show_memory, the full entry body). The readable body
            // is the single human label (stored/deleted/promoted/demoted/
            // showed key); the raw JSON is not a readable result body, so
            // it stays out of the transcript.
            result_summary(tool_name, output).unwrap_or(raw)
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
            outcome: ToolOutcome::from_output_with(
                output,
                tool_name,
                call_input.unwrap_or(&serde_json::Value::Null),
            ),
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
                // whole story, not a phantom result row. An agent-tool result
                // (carries agentId) renders as an inline Subagent fold-group
                // instead of a generic result row.
                if let Some((_, Some(output))) = upd {
                    if let Some(sub) = crate::records::subagent_line(&output, tc.raw_input.as_ref())
                    {
                        out.push(sub);
                    } else {
                        out.push(result_line(id, &tc.title, &output, tc.raw_input.as_ref()));
                    }
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
                    // todo_write's result is boilerplate and the call row is
                    // skipped above, so a late result would orphan. The name
                    // comes from the tools map, which is empty when the call
                    // frame scrolled out of the rebuilt window; recognize the
                    // orphan by its distinctive old_todos field so it never
                    // leaks as a raw {"todos":...} row.
                    let orphan_todo = tool_name.is_empty() && output.get("old_todos").is_some();
                    if tool_name != "todo_write" && !orphan_todo {
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
#[path = "transcript_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transcript_reattach_tests.rs"]
mod reattach_tests;

#[cfg(test)]
#[path = "transcript_silent_tests.rs"]
mod silent_tests;

#[cfg(test)]
#[path = "transcript_acpx_tests.rs"]
mod acpx_tests;

/// After a fresh rebuild (resume or session start) fold each turn's reasoning into a
/// ThoughtFor row per turn that emitted reasoning. A live run records these at
/// Done, but the frame-only rebuilds that fill a blank transcript do not, so
/// a resumed session would otherwise lose every "Thought for Ns (ctrl+o to
/// expand)" entry and its expandable thinking. The thinking stays alongside
/// (hidden rows, kept for /search); the ThoughtFor is the visible expand
/// handle, with reasoning = the turn's concatenated thinking.
pub(crate) fn fold_turn_thoughts(mut lines: Vec<TranscriptLine>) -> Vec<TranscriptLine> {
    let mut out: Vec<TranscriptLine> = Vec::with_capacity(lines.len() + 4);
    let mut reasoning = String::new();
    let mut seq = 0usize;
    for line in lines.drain(..) {
        match &line {
            TranscriptLine::Thinking { text } => {
                reasoning.push_str(text);
                out.push(line);
            }
            TranscriptLine::User(_) => {
                emit_turn_fold(&mut out, &mut reasoning, &mut seq);
                out.push(line);
            }
            _ => out.push(line),
        }
    }
    emit_turn_fold(&mut out, &mut reasoning, &mut seq);
    out
}

fn emit_turn_fold(out: &mut Vec<TranscriptLine>, reasoning: &mut String, seq: &mut usize) {
    if reasoning.is_empty() {
        return;
    }
    *seq += 1;
    out.push(TranscriptLine::ThoughtFor {
        secs: 0,
        reasoning: Some(std::mem::take(reasoning)),
        tool_summary: None,
        turn_id: format!("r{seq}"),
    });
}
