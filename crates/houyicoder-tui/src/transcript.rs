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
use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCall};

use crate::brief::{MEMORY_LABEL_TOOLS, result_summary, tool_call_brief};
use crate::records::{ContextView, ToolOutcome, TranscriptLine};
use crate::transcript::frame_payload::chunk_text;

pub(crate) mod frame_payload;
pub mod snapshot;
mod tool_updates;
#[cfg(test)]
use crate::result_body::count_diff_lines;
use crate::result_body::{
    command_is_silent_success, extract_body, output_has_diff, write_result_body,
};
use tool_updates::{PendingUpdate, ToolRegistry, collect_tool_updates};

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

/// The duration a run-completion record carries. A record without a readable
/// one closes the turn with no duration rather than a claimed zero.
fn recorded_ms(params: &serde_json::Value) -> Option<u64> {
    params.get("ms").and_then(|v| v.as_u64())
}

/// Whether the frame is a run-completion record: the marker that names where a
/// turn ended.
pub(crate) fn is_run_completed(frame: &TranscriptFrame) -> bool {
    matches!(frame, TranscriptFrame::Acpx(n) if n.method == AcpxMethod::ContextRunCompleted)
}

/// Whether the frame is a user message. Both a fresh prompt and a message
/// queued during a turn arrive as one, so the frame alone cannot say which it
/// is.
pub(crate) fn is_user_frame(frame: &TranscriptFrame) -> bool {
    matches!(
        frame,
        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
    )
}

/// Whether the message at the position opens a turn: a message the log does
/// not mark as delivered into the turn already running.
pub(crate) fn opens_turn<F: AsRef<TranscriptFrame>>(frames: &[F], at: usize) -> bool {
    frames.get(at).is_some_and(|sf| is_user_frame(sf.as_ref()))
        && !message_marked_delivered(frames, at)
}

/// Whether a run of frames holds where a turn begins or ends, read in log
/// order: a message that opened a turn, or the record that closed one. A
/// message the log marks as delivered into the turn already running is
/// neither, so a reader seeking backwards through a log reads past it rather
/// than stopping ahead of the turn's real opening — stopping there would leave
/// the fold without the frame that opened the turn it is asked to summarize.
pub fn bounds_turn_in<F: AsRef<TranscriptFrame>>(frames: &[F]) -> bool {
    (0..frames.len()).any(|at| is_run_completed(frames[at].as_ref()) || opens_turn(frames, at))
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
fn message_marked_delivered<F: AsRef<TranscriptFrame>>(frames: &[F], at: usize) -> bool {
    frames
        .get(at + 1..)
        .unwrap_or_default()
        .iter()
        .find(|sf| !matches!(sf.as_ref(), TranscriptFrame::Frontend(_)))
        .map(|sf| sf.as_ref())
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
pub(crate) fn open_turn_before<F: AsRef<TranscriptFrame>>(
    log: &[F],
    start: usize,
) -> Option<usize> {
    let mut search_from = start;
    loop {
        let k = log[..search_from]
            .iter()
            .rposition(|sf| is_user_frame(sf.as_ref()) || is_run_completed(sf.as_ref()))?;
        // A record here closed the turn before this position, so the window
        // opens between turns: no turn to fold.
        if !is_user_frame(log[k].as_ref()) {
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
struct TurnFold<'a, F: AsRef<TranscriptFrame>> {
    /// The whole log, for what a window cannot carry on its own: which turn the
    /// window opens inside, what that turn did before the window began, and
    /// whether the message beside it was delivered into a running turn.
    log: &'a [F],
    /// Log position of the frame that opened the open turn. None when the
    /// window opens between turns — a record behind it ended the one before —
    /// or ahead of any turn at all: frames before the first user message (a
    /// session notice) belong to no turn this projection can summarize.
    opened_at: Option<usize>,
    /// Log position of the newest frame folded into the turn. The row is
    /// named by where the turn ended, which this is once the turn closes.
    ended_at: usize,
    /// The absolute frame index of log[0], added to ended_at so a row name
    /// stays absolute while the walk indexes the log itself.
    abs_base: usize,
    reasoning: String,
    /// Tool calls by tool, in the order the turn first used each.
    tools: Vec<(String, u32)>,
    calls: u32,
}

impl<'a, F: AsRef<TranscriptFrame>> TurnFold<'a, F> {
    /// Fold a window of the log, starting from the turn the window opens
    /// inside (if any). Frames of earlier turns are not folded; the caller
    /// pushes the rows of the turns they closed. What the open turn did before
    /// the window is folded in for facts, with no rows of its own.
    fn new(log: &'a [F], window: Range<usize>, abs_base: usize) -> Self {
        let mut fold = Self {
            log,
            opened_at: open_turn_before(log, window.start),
            ended_at: window.start,
            abs_base,
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
            self.gather(frame.as_ref());
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
                return self.close(recorded_ms(&n.params));
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
                self.reasoning.push_str(chunk_text(chunk));
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
    fn close(&mut self, ms: Option<u64>) -> Option<TranscriptLine> {
        self.opened_at.take()?;
        if self.reasoning.is_empty() && self.calls == 0 {
            return None;
        }
        let reasoning = (!self.reasoning.is_empty()).then(|| std::mem::take(&mut self.reasoning));
        Some(TranscriptLine::ThoughtFor {
            ms,
            reasoning,
            tool_summary: self.tool_summary(),
            turn_id: format!("f{}", self.abs_base + self.ended_at),
        })
    }

    /// The one-line tool summary, grouped by tool with the most-used first.
    /// One kind of tool reads as its own count; mixed kinds keep the breakdown.
    fn tool_summary(&mut self) -> Option<String> {
        if self.calls == 0 {
            return None;
        }
        self.tools
            .sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        if let [(tool, count)] = self.tools.as_slice() {
            return Some(format!("ran {count} {tool}"));
        }
        let parts: Vec<String> = self
            .tools
            .iter()
            .map(|(tool, count)| format!("{count} {tool}"))
            .collect();
        Some(format!("ran {} tools ({})", self.calls, parts.join(", ")))
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
    let frames = &log[start..end];
    let (updates, tools) = collect_tool_updates(frames);
    let mut p = Projector {
        base: start,
        at_end: end == log.len(),
        newest_open,
        updates,
        tools,
        out: Vec::with_capacity(frames.len()),
        late_results: Vec::new(),
        fold: TurnFold::new(log, start..end, abs_base),
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
