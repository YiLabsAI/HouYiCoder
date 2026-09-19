//! Frame-to-transcript projection: rebuild the readable transcript lines from
//! the ordered session/update + acpx frame stream the driver accumulates, and
//! fold each turn's reasoning + tool calls into the one summary row the
//! transcript shows for that turn. The projection derives that row from the
//! turn's own frames rather than from what a host happened to watch live, so
//! a turn replayed from the log renders the row the live turn rendered. A row
//! the frontend raises itself rides the same log, so it keeps its place for
//! the same reason.

use std::ops::Range;

use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate, ToolCallStatus};

use crate::brief::{result_summary, tool_call_brief};
use crate::records::{ContextView, ToolOutcome, TranscriptLine};

/// The transcript-snapshot seam (a loader backed by the durable log) lives
/// as a directory submodule here so its path is transcript::snapshot, not a
/// flat-prefix sibling of this file.
pub mod snapshot;
#[cfg(test)]
use crate::result_body::count_diff_lines;
use crate::result_body::{
    command_is_silent_success, extract_body, output_has_diff, write_result_body,
};

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
    /// for a line the log itself reproduces. The text decides nothing: a
    /// submitted message keeps a prompt echo only until the server sends the
    /// frame for that text, so a line opening with a slash is still the log's.
    pub(crate) fn from_line(line: &TranscriptLine) -> Option<Self> {
        match line {
            TranscriptLine::System(text) => Some(Self::System(text.clone())),
            TranscriptLine::ContextGrid(view) => Some(Self::Context(view.clone())),
            TranscriptLine::Interrupted => Some(Self::Interrupted),
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

/// The text carried by a content chunk, when the chunk wraps a text block.
/// Non-text blocks (Image) have no flat text; an empty string degenerates the
/// line away so a multimodal chunk does not surface as an empty row.
pub fn chunk_text(chunk: &ContentChunk) -> String {
    match &chunk.content {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    }
}

/// Whether the transcript draws this tool call as a chip. A transparent tool
/// draws through its own widget (the checklist) or as the question prompt
/// itself, so the turn summary counts only the calls the user sees.
fn tool_renders_chip(title: &str) -> bool {
    title != "todo_write" && title != "AskUserQuestion"
}

/// The duration a run-completion record carries. A record without a readable
/// one closes the turn with no duration rather than a claimed zero.
fn recorded_secs(params: &serde_json::Value) -> Option<u32> {
    params
        .get("secs")
        .and_then(|v| v.as_u64())
        .map(|s| s.min(u32::MAX as u64) as u32)
}

/// Whether the frame is a run-completion record: the marker that names where a
/// turn ended.
pub(crate) fn is_run_completed(frame: &TranscriptFrame) -> bool {
    matches!(frame, TranscriptFrame::Acpx(n) if n.method == AcpxMethod::ContextRunCompleted)
}

/// Whether the frame is a user message. Both a fresh prompt and a message
/// queued during a turn arrive as one, so the frame alone cannot say which it
/// is.
fn is_user_frame(frame: &TranscriptFrame) -> bool {
    matches!(
        frame,
        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
    )
}

/// Whether the message at the position opens a turn: a message the log does
/// not mark as delivered into the turn already running.
fn opens_turn(frames: &[TranscriptFrame], at: usize) -> bool {
    frames.get(at).is_some_and(is_user_frame) && !message_marked_delivered(frames, at)
}

/// Whether a run of frames holds where a turn begins or ends, read in log
/// order: a message that opened a turn, or the record that closed one. A
/// message the log marks as delivered into the turn already running is
/// neither, so a reader seeking backwards through a log reads past it rather
/// than stopping ahead of the turn's real opening — stopping there would leave
/// the fold without the frame that opened the turn it is asked to summarize.
pub fn bounds_turn_in(frames: &[TranscriptFrame]) -> bool {
    (0..frames.len()).any(|at| is_run_completed(&frames[at]) || opens_turn(frames, at))
}

/// Whether the frame marks the message beside it as belonging to the turn that
/// was already running, rather than a message that opens a turn. A queued
/// interjection, a background child's completion, and the notice that a turn
/// was interrupted all reach the host as user messages, exactly like a fresh
/// prompt, so the message chunk alone cannot say which of the four it is. The
/// projection writes this mark beside the message it belongs to.
fn marks_delivery(frame: &TranscriptFrame) -> bool {
    matches!(
        frame,
        TranscriptFrame::Acpx(n)
            if matches!(
                n.method,
                AcpxMethod::ContextMidTurnInput
                    | AcpxMethod::ContextChildCompleted
                    | AcpxMethod::ContextTurnInterrupted
            )
    )
}

/// Whether the log marks the user message at at as delivered into the turn
/// that was already running. The mark sits beside the message, but a row the
/// frontend raises while the message is in hand can land between the two:
/// the mark is read past those rows, so one raised there cannot leave the
/// message reading as a turn of its own for the rest of the session.
fn message_marked_delivered(frames: &[TranscriptFrame], at: usize) -> bool {
    frames
        .get(at + 1..)
        .unwrap_or_default()
        .iter()
        .find(|f| !matches!(f, TranscriptFrame::Frontend(_)))
        .is_some_and(marks_delivery)
}

/// Where the turn a window starts inside begins: the message that opened it.
/// A user frame is not always an opening — a message the log marks as
/// delivered into the running turn belongs to the turn already running, so
/// the walk continues past it. The search stops at the first undelivered
/// message; a record before it ends the turn instead, and there the window
/// opens between turns, with no turn to fold. A window cut inside a turn
/// still folds that turn, so its row keeps its place rather than vanishing
/// whenever the oldest frames of the view fall inside a turn.
fn open_turn_before(log: &[TranscriptFrame], start: usize) -> Option<usize> {
    let mut search_from = start;
    loop {
        let k = log[..search_from]
            .iter()
            .rposition(|f| is_user_frame(f) || is_run_completed(f))?;
        // A record here closed the turn before this position, so the window
        // opens between turns: no turn to fold.
        if !is_user_frame(&log[k]) {
            return None;
        }
        // A message the log marks as delivered into the running turn did not
        // open it, so the turn begins further back. Reading it as the opening
        // would cut the turn's earlier facts out of the row.
        if opens_turn(log, k) {
            return Some(k);
        }
        search_from = k;
    }
}

/// The facts one turn accumulates for its summary row, gathered as the
/// projection walks that turn's frames. The row is emitted where the turn
/// ends, so the summary lands under the answer it describes.
struct TurnFold<'a> {
    /// The whole log, for what a window cannot carry on its own: which turn the
    /// window opens inside, what that turn did before the window began, and
    /// whether the message beside it was delivered into a running turn.
    log: &'a [TranscriptFrame],
    /// Log position of the frame that opened the open turn. None when the
    /// window opens between turns — a record behind it ended the one before —
    /// or ahead of any turn at all: frames before the first user message (a
    /// session notice) belong to no turn this projection can summarize.
    opened_at: Option<usize>,
    /// Log position of the newest frame folded into the turn. The row is
    /// named by where the turn ended, which this is once the turn closes.
    ended_at: usize,
    reasoning: String,
    /// Tool calls by tool, in the order the turn first used each.
    tools: Vec<(String, u32)>,
    calls: u32,
}

impl<'a> TurnFold<'a> {
    /// Fold a window of the log, starting from the turn the window opens
    /// inside (if any). Frames of earlier turns are not folded; the caller
    /// pushes the rows of the turns they closed. What the open turn did before
    /// the window is folded in for facts, with no rows of its own.
    fn new(log: &'a [TranscriptFrame], window: &Range<usize>) -> Self {
        let mut fold = Self {
            log,
            opened_at: open_turn_before(log, window.start),
            ended_at: window.start,
            reasoning: String::new(),
            tools: Vec::new(),
            calls: 0,
        };
        fold.absorb_ahead(window.start);
        fold
    }

    /// Fold what the open turn did before the window, so the row summarizes the
    /// turn rather than the part of it this window happens to hold. Without it
    /// a window whose oldest frame is the turn's own record finds no reasoning
    /// and no calls to summarize, and drops a row the whole-log read writes.
    /// These frames render no rows of their own here.
    fn absorb_ahead(&mut self, start: usize) {
        let Some(opened_at) = self.opened_at else {
            return;
        };
        let log = self.log;
        for frame in &log[opened_at + 1..start] {
            self.gather(frame);
        }
    }

    /// Whether the message at abs was delivered into the running turn.
    fn delivered_into_turn(&self, abs: usize) -> bool {
        message_marked_delivered(self.log, abs)
    }

    /// Fold one frame into the open turn. A user message opens a turn and ends
    /// the one before it, unless the log marks it as delivered into that turn;
    /// a completion record ends the turn it followed and carries that run's
    /// duration. Returns the row for a turn this frame ended, for the caller to
    /// push where the turn ended.
    fn note(&mut self, abs: usize, frame: &TranscriptFrame) -> Option<TranscriptLine> {
        match frame {
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_)) => {
                let closed = if self.delivered_into_turn(abs) {
                    None
                } else {
                    self.close(None)
                };
                if self.opened_at.is_none() {
                    self.opened_at = Some(abs);
                    self.ended_at = abs;
                    self.reasoning.clear();
                    self.tools.clear();
                    self.calls = 0;
                }
                return closed;
            }
            TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(_))
            | TranscriptFrame::Session(SessionUpdate::ToolCall(_)) => self.gather(frame),
            TranscriptFrame::Acpx(n) if n.method == AcpxMethod::ContextRunCompleted => {
                self.ended_at = abs;
                return self.close(recorded_secs(&n.params));
            }
            // A row the frontend raises is not a fact of the turn it sits in:
            // it does not move where the turn ended, which names the row.
            TranscriptFrame::Frontend(_) => return None,
            _ => {}
        }
        self.ended_at = abs;
        None
    }

    /// Collect what one frame of the open turn contributes to its summary: its
    /// reasoning text, and the tool calls it made. Read for the frames the
    /// window holds and for those it reads back to reach the turn's opening.
    fn gather(&mut self, frame: &TranscriptFrame) {
        if self.opened_at.is_none() {
            return;
        }
        match frame {
            TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(chunk)) => {
                self.reasoning.push_str(&chunk_text(chunk));
            }
            TranscriptFrame::Session(SessionUpdate::ToolCall(call))
                if tool_renders_chip(&call.title) =>
            {
                match self.tools.iter_mut().find(|(tool, _)| tool == &call.title) {
                    Some(slot) => slot.1 += 1,
                    None => self.tools.push((call.title.clone(), 1)),
                }
                self.calls += 1;
            }
            _ => {}
        }
    }

    /// Close the open turn and return its row: None when no turn is open, or
    /// when the turn has nothing to summarize. A plain reply renders no row
    /// whose expand affordance would lead to nothing.
    fn close(&mut self, secs: Option<u32>) -> Option<TranscriptLine> {
        self.opened_at.take()?;
        if self.reasoning.is_empty() && self.calls == 0 {
            return None;
        }
        let reasoning = (!self.reasoning.is_empty()).then(|| std::mem::take(&mut self.reasoning));
        Some(TranscriptLine::ThoughtFor {
            secs,
            reasoning,
            tool_summary: self.tool_summary(),
            turn_id: format!("f{}", self.ended_at),
        })
    }

    /// The one-line tool summary ("ran 3 tools (2 bash, 1 grep)"), grouped by
    /// tool with the most-used first. None when the turn called none.
    fn tool_summary(&mut self) -> Option<String> {
        if self.calls == 0 {
            return None;
        }
        self.tools
            .sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let parts: Vec<String> = self
            .tools
            .iter()
            .map(|(tool, count)| format!("{count} {tool}"))
            .collect();
        let noun = if self.calls == 1 { "tool" } else { "tools" };
        Some(format!("ran {} {noun} ({})", self.calls, parts.join(", ")))
    }
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
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub fn transcript_from_frames(
    log: &[TranscriptFrame],
    window: Range<usize>,
    newest_open: bool,
) -> Vec<TranscriptLine> {
    let frames = &log[window.clone()];
    let base = window.start;
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
    let mut fold = TurnFold::new(log, &window);
    for (i, f) in frames.iter().enumerate() {
        // The turn fold runs first: the row of a turn this frame ends lands
        // ahead of this frame's own lines, which is where the turn ended.
        if let Some(row) = fold.note(base + i, f) {
            out.push(row);
        }
        match f {
            TranscriptFrame::Frontend(row) => out.push(row.line()),
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
    // The window's last turn. A log written before the completion record
    // existed ends its final turn nowhere else, so this is where that turn's
    // summary row comes from. The window must reach the end of the log for
    // this: a window cut mid-log keeps its last turn open, whose end the rest
    // of the log still holds, and a turn nothing has stopped running yet
    // yields no summary at all.
    if !newest_open
        && window.end == log.len()
        && let Some(row) = fold.close(None)
    {
        out.push(row);
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
        // No matching call row in the window: the call compacted out, so the
        // result has no place — drop it rather than stranding it at the tail.
        if let Some(pos) = insert_at {
            out.insert(pos, result_line(&id, &tool_name, &output, None));
        }
    }
    out
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
