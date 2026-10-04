//! Frame-to-transcript projection: rebuild the readable transcript lines from
//! the ordered session/update + acpx frame stream the driver accumulates, and
//! fold each turn's reasoning + tool calls into the one summary row the
//! transcript shows for that turn. The projection derives that row from the
//! turn's own frames rather than from what a host happened to watch live, so
//! a turn replayed from the log renders the row the live turn rendered. A row
//! the frontend raises itself rides the same log, so it keeps its place for
//! the same reason.

use std::ops::Range;

use houyicoder_protocol::acpx::AcpxNotification;
use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCall};

use crate::brief::{MEMORY_LABEL_TOOLS, result_summary, tool_call_brief};
use crate::records::{ContextView, ToolOutcome, TranscriptLine};
use crate::transcript::frame_payload::chunk_text;

pub(crate) mod frame_payload;
pub mod snapshot;
mod tool_updates;
mod turn_fold;
#[cfg(test)]
use crate::result_body::count_diff_lines;
use crate::result_body::{
    command_is_silent_success, extract_body, output_has_diff, write_result_body,
};
use tool_updates::{PendingUpdate, ToolRegistry, collect_tool_updates};
use turn_fold::{RowNames, TurnFold};

pub use turn_fold::bounds_turn_in;
pub(crate) use turn_fold::is_user_frame;

/// One frame of the turn stream, preserved in arrival order so the
/// transcript rebuild keeps the time-ordered interleave of session/update
/// chunks and acpx/context/* audit notifications (a compaction checkpoint
/// lands between the tool calls that bracketed it, not at the tail). The
/// driver accumulates the server's frames as it pushes them, and the
/// frontend appends its own rows to the same log; the transcript is a
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
    /// A row the frontend raises on its own account, listed in FrontendRow:
    /// no server frame carries it, so the frontend puts it in this log at the
    /// position it happened. Every view of the log renders it there — the live
    /// turn, a window that slid past older frames, scrollback loading frames
    /// above it — and a view that no longer covers its frame shows it no more.
    Frontend(FrontendRow),
}

/// A row the frontend raises on its own account. It renders through the same
/// projection as the server's frames, so it lands where it was raised and
/// stays there while the window slides, rather than drifting to the head of
/// whatever rows a rebuild happens to keep.
#[derive(Debug, Clone)]
pub enum FrontendRow {
    /// A system line: command feedback, a notice, a warning, a failure.
    System(String),
    /// A /context breakdown rendered inline as conversation content.
    Context(ContextView),
    /// The abort notice, welded under the message the abort cut off.
    Interrupted,
    /// The notice that the interrupted submission went back to the input box.
    InputRestored,
    /// The echo of a submitted command.
    Echo(String),
}

impl FrontendRow {
    /// The row the frontend raises for a line no server frame carries, or None
    /// for a line the log itself reproduces. A user message the frontend echoes
    /// back as a prompt row is tentative: it renders from its own Echo frame
    /// until the server sends the UserMessageChunk for the same text, at which
    /// point push_frame drops the Echo so the row renders once from the server
    /// frame. The text decides nothing about routing: a slash echo is still
    /// frontend-raised, and a plain message echo is too.
    pub(crate) fn from_line(line: &TranscriptLine) -> Option<Self> {
        match line {
            TranscriptLine::System(text) => Some(Self::System(text.clone())),
            TranscriptLine::ContextGrid(view) => Some(Self::Context(view.clone())),
            TranscriptLine::Interrupted => Some(Self::Interrupted),
            TranscriptLine::User(text) => Some(Self::Echo(text.clone())),
            _ => None,
        }
    }

    /// The row this frame renders as.
    fn line(&self) -> TranscriptLine {
        match self {
            Self::System(text) => TranscriptLine::System(text.clone()),
            Self::Context(view) => TranscriptLine::ContextGrid(view.clone()),
            Self::Interrupted => TranscriptLine::Interrupted,
            Self::InputRestored => TranscriptLine::System("input restored".into()),
            Self::Echo(text) => TranscriptLine::User(text.clone()),
        }
    }
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

/// A frame paired with the event seq that produced it, when the frame came
/// from the server stream. Frontend-raised rows and test-built frames carry
/// no seq: they never anchor a block alone (they attach to the surrounding
/// turn), and the block layer assigns them a local anchor instead.
#[derive(Debug, Clone)]
pub struct SequencedFrame {
    pub seq: Option<houyicoder_protocol::envelope::EventSeq>,
    pub frame: TranscriptFrame,
}

impl From<TranscriptFrame> for SequencedFrame {
    fn from(frame: TranscriptFrame) -> Self {
        Self { seq: None, frame }
    }
}

impl SequencedFrame {
    /// The frame without its seq, for callers that read the projection shape.
    pub fn frame(&self) -> &TranscriptFrame {
        &self.frame
    }
}

/// Lets the projection read a frame log stored as either bare TranscriptFrame
/// (test fixtures, the legacy storage) or SequencedFrame (the Transcript store
/// that carries event seq). Both slice shapes feed the same projector without a
/// per-call conversion, so a test that builds a Vec of TranscriptFrame and a
/// rebuild that holds the live SequencedFrame log call the same entry.
impl AsRef<TranscriptFrame> for SequencedFrame {
    fn as_ref(&self) -> &TranscriptFrame {
        &self.frame
    }
}

impl AsRef<TranscriptFrame> for TranscriptFrame {
    fn as_ref(&self) -> &TranscriptFrame {
        self
    }
}

/// Whether the transcript draws this tool call as a chip. A transparent tool
/// draws through its own widget (the checklist) or as the question prompt
/// itself, so the turn summary counts only the calls the user sees.
fn tool_renders_chip(title: &str) -> bool {
    title != "todo_write" && title != "AskUserQuestion"
}

/// Rebuild the transcript from the ordered frame log the driver accumulated.
/// Each SessionUpdate maps to one TranscriptLine; the acpx audit kinds the
/// transcript surfaces (compaction, summary) become System lines; the
/// meta-user nudge + permission-decision audit are dropped (control-only);
/// a row the frontend raised renders where its own frame sits. Tool-call
/// outcomes are resolved in a first pass from the matching ToolCallUpdate so
/// the call chip colors by outcome.
///
/// log is the whole frame log and window is the part of it to render. Only the
/// window's frames become lines, but two of the facts those lines are built on
/// lie outside it when the window does not start at the log's beginning: the
/// turn an opening frame sits inside, and the mark that says a user message was
/// delivered into a running turn. Both are read from the log, so a window and
/// the whole log agree about where turns begin and end.
///
/// The window's start is the log position of its first frame: a turn's summary
/// row is named by where that turn ended in the log, so the name holds still
/// while the window slides (a window that dropped its oldest frames would
/// otherwise renumber every row and detach the expand state from its row).
/// newest_open says whether the newest turn may still be running: a turn still
/// going yields no row, since a half-accumulated summary would otherwise render
/// as a finished one.
pub fn transcript_from_frames<F: AsRef<TranscriptFrame>>(
    log: &[F],
    window: Range<usize>,
    newest_open: bool,
) -> Vec<TranscriptLine> {
    transcript_from_frames_at(log, 0, window, newest_open)
}

/// Project a window of frames to lines when the log's first frame sits at an
/// absolute frame index other than zero. The window is an absolute frame range
/// and abs_base is the absolute index of log[0], so a window onto a drained log
/// still names its turns by absolute position: a turn's row identity stays put
/// when the resident window's front advances, and expand state stays attached
/// to it.
pub fn transcript_from_frames_at<F: AsRef<TranscriptFrame>>(
    log: &[F],
    abs_base: usize,
    window: Range<usize>,
    newest_open: bool,
) -> Vec<TranscriptLine> {
    let start = window.start.saturating_sub(abs_base);
    let end = window.end.saturating_sub(abs_base).min(log.len());
    render_window(
        log,
        RowNames::FrameIndex { abs_base },
        start..end,
        newest_open,
    )
}

/// Render a durable log read whose frames carry durable names, one per frame
/// in log order. A turn's summary row takes the name at the frame that closed
/// the turn, so the same turn keeps its row identity across reads that start
/// at different points: a position-based name cannot promise that for a
/// windowed read, whose positions shift with the window and collide with the
/// live frame-index names the resident transcript already carries. The reader
/// owns the name format, since the log's identity type is the reader's; the
/// window is relative to log and names must run parallel to it.
pub fn transcript_from_named_frames<F: AsRef<TranscriptFrame>>(
    log: &[F],
    names: &[String],
    window: Range<usize>,
    newest_open: bool,
) -> Vec<TranscriptLine> {
    assert_eq!(
        names.len(),
        log.len(),
        "row names must run parallel to the frame log"
    );
    let end = window.end.min(log.len());
    render_window(
        log,
        RowNames::EventName(names),
        window.start..end,
        newest_open,
    )
}

/// The shared walk behind both entries: window is relative to log, and names
/// says where the folded rows take their identity.
fn render_window<'a, F: AsRef<TranscriptFrame>>(
    log: &'a [F],
    names: RowNames<'a>,
    window: Range<usize>,
    newest_open: bool,
) -> Vec<TranscriptLine> {
    let frames = &log[window.clone()];
    let (updates, tools) = collect_tool_updates(frames);
    let mut p = Projector {
        base: window.start,
        at_end: window.end == log.len(),
        newest_open,
        updates,
        tools,
        out: Vec::with_capacity(frames.len()),
        late_results: Vec::new(),
        fold: TurnFold::new(log, window, names),
    };
    p.run(frames);
    p.finish();
    p.out
}

/// Build the single result row for a tool call from its output. The body is
/// tool-specific: Read shows a one-line summary (content stays in the frame
/// log for the model, never the transcript — dumping it flooded the view and
/// enabled the duplication bug, bug-log #27); Bash shows raw stdout (its
/// summary is the first stdout line, which the raw body already starts with, so
/// prepending it duplicates line 1); a silent Bash success renders "done"
/// rather than an empty "(no output)" placeholder that reads as failure; the
/// memory tools render a single human label (the raw JSON is not a readable
/// body); Write renders the chip plus the written content pulled from the
/// call's input (folding is the render layer's job via tool_rows, not baked
/// in). Other tools keep summary + raw when both are present.
fn tool_result_line(
    id: &str,
    tool_name: &str,
    output: &serde_json::Value,
    call_input: Option<&serde_json::Value>,
) -> TranscriptLine {
    let out_str = output.to_string();
    let raw = extract_body(&out_str);
    let body = if tool_name == "read" {
        if output.get("error").is_some() {
            raw
        } else {
            result_summary(tool_name, output).unwrap_or_default()
        }
    } else if tool_name == "bash" {
        if raw.is_empty() && command_is_silent_success(call_input, output) {
            "done".to_string()
        } else {
            raw
        }
    } else if MEMORY_LABEL_TOOLS.contains(&tool_name) {
        result_summary(tool_name, output).unwrap_or(raw)
    } else if tool_name == "write" {
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
}

/// The state the projection accumulates while walking a window of frames: the
/// tool-call updates paired in the first pass, the lines emitted so far, the
/// late-arriving results awaiting reposition, and the turn fold. Held in a
/// struct so the walk is a sequence of method calls on shared state rather
/// than one long function body.
struct Projector<'a, F: AsRef<TranscriptFrame>> {
    base: usize,
    at_end: bool,
    newest_open: bool,
    updates: Vec<PendingUpdate>,
    tools: ToolRegistry,
    out: Vec<TranscriptLine>,
    late_results: Vec<(String, String, serde_json::Value)>,
    fold: TurnFold<'a, F>,
}

impl<'a, F: AsRef<TranscriptFrame>> Projector<'a, F> {
    /// Walk the window's frames in order, folding each into the open turn and
    /// emitting its lines. The fold runs first so a turn's summary row lands
    /// ahead of the frame that ended it.
    fn run(&mut self, frames: &[F]) {
        for (i, f) in frames.iter().enumerate() {
            let abs = self.base + i;
            if let Some(row) = self.fold.note(abs, f.as_ref()) {
                self.out.push(row);
            }
            self.emit(f.as_ref());
        }
        // The window's last turn. A log written before the completion record
        // existed ends its final turn nowhere else, so this is where that
        // turn's summary row comes from. The window must reach the end of the
        // log for this: a window cut mid-log keeps its last turn open, whose
        // end the rest of the log still holds, and a turn nothing has stopped
        // running yet yields no summary at all.
        if !self.newest_open
            && self.at_end
            && let Some(row) = self.fold.close(None)
        {
            self.out.push(row);
        }
    }

    /// FIFO-consume the first update whose id matches, removing it so the next
    /// call with the same id pairs with its own update (not the last insert).
    /// Correctness relies on a call_id uniqueness invariant established at the
    /// provider boundary (unique_id_gen in openai_compat.rs assigns empty and
    /// duplicate-within-response ids before any frame is built): with unique
    /// ids each id has exactly one call and one update, so FIFO-by-arrival
    /// degenerates to identity pairing regardless of completion order. If a
    /// duplicate id ever reaches here, the earlier call silently steals the
    /// first-arrived result for that id (pending_approvals and apply_decisions
    /// in agent/mod.rs mis-route the same invariant the same way).
    fn take_update(
        &mut self,
        id: &str,
    ) -> Option<(Option<ToolOutcome>, Option<serde_json::Value>)> {
        let pos = self.updates.iter().position(|(cid, _, _)| cid == id)?;
        let (_, oc, out) = self.updates.remove(pos);
        Some((oc, out))
    }

    /// Emit the lines one frame contributes at this log position.
    fn emit(&mut self, f: &TranscriptFrame) {
        use houyicoder_protocol::acpx::AcpxMethod;
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        match f {
            TranscriptFrame::Frontend(row) => self.out.push(row.line()),
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(chunk)) => {
                self.out
                    .push(TranscriptLine::User(chunk_text(chunk).to_string()));
            }
            TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(chunk)) => {
                let text = chunk_text(chunk);
                if !text.is_empty() {
                    self.out.push(TranscriptLine::Agent(text.to_string()));
                }
            }
            TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(chunk)) => {
                self.out.push(TranscriptLine::Thinking {
                    text: chunk_text(chunk).to_string(),
                });
            }
            TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) => self.emit_tool_call(tc),
            TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) => {
                // A late-arriving result (its ToolCall frame already passed,
                // so take_update at the call found nothing then). Updates
                // whose ToolCall was present AND already consumed return None
                // here and skip. Do NOT push inline at the arrival position —
                // that detaches the result from its call and lets a thought
                // interleave between them. Defer; the reposition pass attaches
                // each late result right after its call row.
                let id = &upd.tool_call_id.0;
                if let Some((_, Some(output))) = self.take_update(id) {
                    let (tool_name, _) = self.tools.get(id).cloned().unwrap_or_default();
                    // todo_write's result is boilerplate and the call row is
                    // skipped, so a late result would orphan. The name comes
                    // from the tools map, which is empty when the call frame
                    // scrolled out of the rebuilt window; recognize the orphan
                    // by its distinctive old_todos field so it never leaks as a
                    // raw {"todos":...} row.
                    let orphan_todo = tool_name.is_empty() && output.get("old_todos").is_some();
                    if tool_name != "todo_write" && !orphan_todo {
                        self.late_results.push((id.clone(), tool_name, output));
                    }
                }
            }
            TranscriptFrame::Acpx(n) => match n.method {
                AcpxMethod::ContextCompactionBoundary => {
                    self.out
                        .push(TranscriptLine::System("compaction checkpoint".to_string()));
                }
                AcpxMethod::ContextSummary => {
                    let text = n
                        .params
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.out
                        .push(TranscriptLine::System(format!("summary: {text}")));
                }
                // Audit-only: the meta-user nudge is a control message the
                // runner injects (never authored by the human); the verdict is
                // already visible via the approval card. Both stay out of the
                // readable transcript.
                AcpxMethod::ContextMetaUser | AcpxMethod::ContextPermissionDecision => {}
                _ => {}
            },
            // A future SessionUpdate variant the transcript does not render
            // yet (Plan, SessionInfoUpdate, ...) is ignored so the rebuild
            // never fails on a shape the frontend does not model.
            _ => {}
        }
    }

    /// Emit the call chip and result row for a ToolCall frame. todo_write
    /// renders only via the checklist widget, so both its rows are skipped
    /// here (the frame stays in the log for the widget + verdict cursor). The
    /// call row is skipped for the transparent HITL question tool — its
    /// answer row below still renders. The single result row groups under its
    /// call; an agent-tool result (carries agentId) renders as an inline
    /// Subagent fold-group instead of a generic result row.
    fn emit_tool_call(&mut self, tc: &ToolCall) {
        let id = &tc.tool_call_id.0;
        let upd = self.take_update(id);
        if tc.title == "todo_write" {
            return;
        }
        if tool_renders_chip(&tc.title) {
            let outcome = upd
                .as_ref()
                .and_then(|(oc, _)| *oc)
                .unwrap_or(ToolOutcome::Running);
            let input = tc
                .raw_input
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            self.out.push(TranscriptLine::Tool {
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
        if let Some((_, Some(output))) = upd {
            if let Some(sub) = crate::records::subagent_line(&output, tc.raw_input.as_ref()) {
                self.out.push(sub);
            } else {
                self.out.push(tool_result_line(
                    id,
                    &tc.title,
                    &output,
                    tc.raw_input.as_ref(),
                ));
            }
        }
    }

    /// Reposition pass: attach each late result right after its matching call
    /// row so a result that arrived after a thought pulls back to its call
    /// (preserving call+result adjacency + input order). A late result whose
    /// call row is absent (compacted) falls through to the tail. Forward
    /// search for the first call row with the matching id; the harness ships
    /// one durable update per call, so at most one late result per id lands
    /// here (an orphan whose call was compacted out), and the first match is
    /// the right one. Skip past any result rows already placed for THIS
    /// call_id so multiple late results for one call stack in arrival order
    /// without detaching an edit's diff from its call.
    fn finish(&mut self) {
        for (id, tool_name, output) in std::mem::take(&mut self.late_results) {
            let mut insert_at: Option<usize> = None;
            for (i, line) in self.out.iter().enumerate() {
                if let TranscriptLine::Tool { name, call_id, .. } = line
                    && name != "result"
                    && call_id == &id
                {
                    let mut j = i + 1;
                    while j < self.out.len()
                        && matches!(
                            &self.out[j],
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
            // No matching call row in the window: the call compacted out, so
            // the result has no place — drop it rather than stranding it at the
            // tail.
            if let Some(pos) = insert_at {
                self.out
                    .insert(pos, tool_result_line(&id, &tool_name, &output, None));
            }
        }
    }
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
