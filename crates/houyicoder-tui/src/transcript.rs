//! Frame-to-row fold: rebuild the readable transcript lines from
//! the ordered session/update + acpx frame stream the driver accumulates, and
//! fold each turn's reasoning + tool calls into the one summary row the
//! transcript shows for that turn. The fold derives that row from the
//! turn's own frames rather than from what a host happened to watch live, so
//! a turn replayed from the log renders the row the live turn rendered. A row
//! the frontend raises itself rides the same log, so it keeps its place for
//! the same reason.

pub(crate) mod app_scroll;
pub mod blocks;
pub(crate) mod brief;
pub(crate) mod cursors;
pub(crate) mod fold;
pub(crate) mod git_op;
pub(crate) mod history_state;
pub(crate) mod markdown;
pub(crate) mod parked_view_states;
pub mod records;
pub mod redaction;
pub mod result_body;
pub mod row_id;
pub mod rows;
pub(crate) mod scan_chunk;
pub(crate) mod scan_state;
pub(crate) mod scroll;
pub(crate) mod search_view;
pub mod store;
pub mod view;

use std::ops::Range;

use houyicoder_protocol::acpx::AcpxNotification;
use houyicoder_protocol::envelope::{DurableEventId, EventSeq};
use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCall};

use crate::brief::tool_call_brief;
use crate::records::{ContextView, ToolOutcome, TranscriptLine};
use crate::row_id::RowId;
use crate::transcript::frame_payload::chunk_text;
use crate::transcript::rows::NamedRows;

mod call_rows;
pub(crate) mod frame_payload;
pub mod snapshot;
mod tool_updates;
mod turn_fold;
#[cfg(test)]
use crate::result_body::count_diff_lines;
use call_rows::{call_out_of_view_row, tool_renders_chip, tool_result_line};
use tool_updates::{PendingUpdate, ToolRegistry, collect_tool_updates};
use turn_fold::{RowNames, TurnFold};

pub use turn_fold::TurnCheckpoint;

use crate::brief::tool_user_facing_name;
use crate::records::subagent_line;
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::envelope::ChildTranscriptFrame as C;
use houyicoder_protocol::envelope::ChildTranscriptFrame;
use houyicoder_protocol::tool::tool_invocation;
use serde_json::Value;
use std::mem::take;
pub use turn_fold::bounds_turn_in;
pub(crate) use turn_fold::delivered_marker_at;
pub(crate) use turn_fold::is_user_frame;
pub(crate) use turn_fold::starts_inside_turn;
pub(crate) use turn_fold::turn_state_from_frames_at;

/// One frame of the turn stream, preserved in arrival order so the
/// transcript rebuild keeps the time-ordered interleave of session/update
/// chunks and acpx/context/* audit notifications (a compaction checkpoint
/// lands between the tool calls that bracketed it, not at the tail). The
/// driver accumulates the server's frames as it pushes them, and the
/// frontend appends its own rows to the same log; the transcript is a
/// faithful fold of that ordered stream, never a stub.
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
/// fold as the server's frames, so it lands where it was raised and
/// stays there while the window slides, rather than drifting to the head of
/// whatever rows a rebuild happens to keep.
#[derive(Debug, Clone)]
pub enum FrontendRow {
    /// A system line: command feedback, a notice, a warning, a failure.
    System(String),
    /// A /context breakdown rendered inline as conversation content. Boxed:
    /// the view is far larger than any other row, and every row is cloned on
    /// a rebuild.
    Context(Box<ContextView>),
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
            TranscriptLine::ContextGrid(view) => Some(Self::Context(Box::new(view.clone()))),
            TranscriptLine::Interrupted => Some(Self::Interrupted),
            TranscriptLine::User(text) => Some(Self::Echo(text.clone())),
            _ => None,
        }
    }

    /// The row this frame renders as.
    fn line(&self) -> TranscriptLine {
        match self {
            Self::System(text) => TranscriptLine::System(text.clone()),
            Self::Context(view) => TranscriptLine::ContextGrid((**view).clone()),
            Self::Interrupted => TranscriptLine::Interrupted,
            Self::InputRestored => TranscriptLine::System("input restored".into()),
            Self::Echo(text) => TranscriptLine::User(text.clone()),
        }
    }
}

/// Convert a fetched child frame to the live-frame shape the fold
/// reads, so child rows render through the same pipeline as the parent flow.
impl From<ChildTranscriptFrame> for TranscriptFrame {
    fn from(frame: ChildTranscriptFrame) -> Self {
        match frame {
            C::Session(u) => TranscriptFrame::Session(u),
            C::Acpx(n) => TranscriptFrame::Acpx(n),
        }
    }
}

/// A transcript frame plus the stream metadata its envelope carried. A frame
/// from the server stream holds the envelope's event seq — the resume
/// position within one connection, never a row identity, because sequence
/// numbering restarts with every service process — and, when the payload was
/// mapped from a durable log entry, that entry's durable event identity,
/// which is stable across reconnects. Frontend-raised rows and test-built
/// frames carry neither.
#[derive(Debug, Clone)]
pub struct SequencedFrame {
    pub seq: Option<EventSeq>,
    pub event_id: Option<DurableEventId>,
    pub frame: TranscriptFrame,
}

impl From<TranscriptFrame> for SequencedFrame {
    fn from(frame: TranscriptFrame) -> Self {
        Self {
            seq: None,
            event_id: None,
            frame,
        }
    }
}

impl SequencedFrame {
    /// The frame without its stream metadata, for callers that read the fold shape.
    pub fn frame(&self) -> &TranscriptFrame {
        &self.frame
    }
}

/// Lets the frame walk read a log stored as either bare TranscriptFrame (test
/// fixtures) or SequencedFrame (the Transcript store that carries the stream
/// metadata). The walk and the turn fold take a slice of anything convertible
/// this way, so both shapes reach them by reference and neither pays a
/// conversion: a test holding a Vec of bare frames and a rebuild holding the
/// live log run the same walk.
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
///
/// log holds bare frames, so no frame carries an entry identity and the rows
/// this entry builds are named by position. A log whose frames carry entry
/// identities goes through transcript_from_frames_at, which keeps them;
/// accepting one here would drop every identity on the way in.
pub fn transcript_from_frames(
    log: &[TranscriptFrame],
    window: Range<usize>,
    newest_open: bool,
) -> Vec<TranscriptLine> {
    walk_window(
        log,
        RowNames::Position { abs_base: 0 },
        resident_window(0, log.len(), window),
        newest_open,
        None,
    )
    .0
    .into_lines()
}

/// Derive the rows of a window when the log's first frame sits at an absolute
/// frame index other than zero. The window is an absolute frame range and
/// abs_base is the absolute index of log[0]. A turn's summary row takes the
/// identity of the frame that closed the turn: the durable entry's own identity
/// when the frame carries one, else the absolute position, which stays put when
/// the resident window's front drains, so expand state stays attached to its row.
/// Every row carries the identity of the frame that produced it, so a read
/// that joins windows matches rows by identity rather than by text.
///
/// carried is the state the read of the previous window ended inside a turn
/// with; the read continues that turn instead of folding it again, which is
/// what lets a window summarize a turn whose earlier frames the log no longer
/// holds. The state at this window's end comes back for the next read. With
/// nothing carried the fold reads back what it can of the turn from the log it
/// was handed.
pub fn transcript_from_frames_at(
    log: &[SequencedFrame],
    abs_base: usize,
    window: Range<usize>,
    newest_open: bool,
    carried: Option<TurnCheckpoint>,
) -> (NamedRows, TurnCheckpoint) {
    walk_window(
        log,
        RowNames::Live { log, abs_base },
        resident_window(abs_base, log.len(), window),
        newest_open,
        carried,
    )
}

/// The part of a log an absolute frame range reaches, when the log's first
/// frame sits at abs_base and its front may already have drained. The walk
/// takes a range into the slice it holds, so an absolute range has to be
/// shifted onto the resident frames and clamped to what is still there.
fn resident_window(abs_base: usize, log_len: usize, window: Range<usize>) -> Range<usize> {
    let start = window.start.saturating_sub(abs_base);
    let end = window.end.saturating_sub(abs_base).min(log_len);
    start..end
}

/// Render a durable log read whose frames carry row identities, one per frame
/// in log order. A turn's summary row takes the identity at the frame that
/// closed the turn, so the same turn keeps its row identity across reads that
/// start at different points: a position-based name cannot promise that for a
/// windowed read, whose positions shift with the window. Every row carries the
/// identity of the frame that produced it, so a read that joins windows matches
/// rows by identity rather than by text. The reader derives the identities
/// through the same function the live path uses, so one entry names one row in
/// both; the window is relative to log and the identities must run parallel to
/// it.
pub fn transcript_from_named_frames<F: AsRef<TranscriptFrame>>(
    log: &[F],
    names: &[RowId],
    window: Range<usize>,
    newest_open: bool,
    carried: Option<TurnCheckpoint>,
) -> (NamedRows, TurnCheckpoint) {
    assert_eq!(
        names.len(),
        log.len(),
        "row names must run parallel to the frame log"
    );
    let end = window.end.min(log.len());
    walk_window(
        log,
        RowNames::EventName(names),
        window.start..end,
        newest_open,
        carried,
    )
}

/// The shared walk behind every entry that derives rows from a frame log:
/// window is relative to log, and names says where each row takes its
/// identity. The state the walk ends inside a turn with comes back beside the
/// rows, for the read of the window that continues that turn.
fn walk_window<'a, F: AsRef<TranscriptFrame>>(
    log: &'a [F],
    names: RowNames<'a>,
    window: Range<usize>,
    newest_open: bool,
    carried: Option<TurnCheckpoint>,
) -> (NamedRows, TurnCheckpoint) {
    let frames = &log[window.clone()];
    let (updates, tools) = collect_tool_updates(frames);
    let mut p = LineBuilder {
        base: window.start,
        at_end: window.end == log.len(),
        newest_open,
        updates,
        tools,
        out: Vec::with_capacity(frames.len()),
        ids: Vec::with_capacity(frames.len()),
        late_results: Vec::new(),
        fold: TurnFold::new(log, window, names, carried),
    };
    p.run(frames);
    // The reposition pass runs before the trailing close so a result row it
    // appends falls inside the turn it belongs to, and the summary row the close
    // writes stays the last row of the turn it summarizes.
    p.finish();
    // The state handed to the next window describes the turn this window's
    // frames left open, so it is read before the trailing close below. That
    // close answers a question about what to render — whether a log that ends
    // without a record should show the turn it stops inside — while the state
    // is a fact of the frames: a turn the log merely ends at is still the turn
    // the read that continues the log must summarize.
    let exit = p.fold.checkpoint();
    p.close_trailing_turn();
    (NamedRows::from_parts(p.out, p.ids), exit)
}

/// A result the walk set aside rather than place where it arrived: the
/// output, the tool it belongs to, and the identity of the update frame that
/// carried it, which names the row it becomes. The reposition pass attaches it
/// beside the call row its call id names, and a call that draws no row of its
/// own leaves the result where the pass appends it.
struct LateResult {
    row: RowId,
    call_id: String,
    tool_name: String,
    output: Value,
}

/// The state the walk accumulates while building a window's lines: the
/// tool-call updates paired in the first pass, the lines emitted so far each
/// beside its identity, the late-arriving results awaiting reposition, and
/// the turn fold. Held in a struct so the walk is a sequence of method calls
/// on shared state rather than one long function body.
struct LineBuilder<'a, F: AsRef<TranscriptFrame>> {
    base: usize,
    at_end: bool,
    newest_open: bool,
    updates: Vec<PendingUpdate>,
    tools: ToolRegistry,
    out: Vec<TranscriptLine>,
    ids: Vec<RowId>,
    late_results: Vec<LateResult>,
    fold: TurnFold<'a, F>,
}

impl<'a, F: AsRef<TranscriptFrame>> LineBuilder<'a, F> {
    /// Walk the window's frames in order, folding each into the open turn and
    /// emitting its lines. The fold runs first so a turn's summary row lands
    /// ahead of the frame that ended it.
    fn run(&mut self, frames: &[F]) {
        for (i, f) in frames.iter().enumerate() {
            let abs = self.base + i;
            if let Some((row, id)) = self.fold.note(abs, f.as_ref()) {
                self.out.push(row);
                self.ids.push(id);
            }
            self.emit(abs, f.as_ref());
        }
    }

    /// Close the window's last turn when the log ends with no record for it. A
    /// log written before the completion record existed ends its final turn
    /// nowhere else, so this is where that turn's summary row comes from. The
    /// window must reach the end of the log for this: a window cut mid-log
    /// keeps its last turn open, whose end the rest of the log still holds, and
    /// a turn nothing has stopped running yet yields no summary at all.
    fn close_trailing_turn(&mut self) {
        if !self.newest_open
            && self.at_end
            && let Some((row, id)) = self.fold.close(None)
        {
            self.out.push(row);
            self.ids.push(id);
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
    fn take_update(&mut self, id: &str) -> Option<(Option<ToolOutcome>, Option<Value>)> {
        let pos = self.updates.iter().position(|(cid, _, _)| cid == id)?;
        let (_, oc, out) = self.updates.remove(pos);
        Some((oc, out))
    }

    /// Emit the lines one frame contributes at this log position, each named
    /// by the identity of the frame that produced it: a frame can yield
    /// several rows (a call chip beside its result), and they share the
    /// identity because they share the frame.
    fn emit(&mut self, abs: usize, f: &TranscriptFrame) {
        let id = self.fold.name_at(abs);
        match f {
            TranscriptFrame::Frontend(row) => {
                self.out.push(row.line());
                self.ids.push(id);
            }
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(chunk)) => {
                self.out
                    .push(TranscriptLine::User(chunk_text(chunk).to_string()));
                self.ids.push(id);
            }
            TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(chunk)) => {
                let text = chunk_text(chunk);
                if !text.is_empty() {
                    self.out.push(TranscriptLine::Agent(text.to_string()));
                    self.ids.push(id);
                }
            }
            TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(chunk)) => {
                self.out.push(TranscriptLine::Thinking {
                    text: chunk_text(chunk).to_string(),
                });
                self.ids.push(id);
            }
            TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) => {
                self.emit_tool_call(tc, &id);
            }
            TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) => {
                // A late-arriving result (its ToolCall frame already passed,
                // so take_update at the call found nothing then). Updates
                // whose ToolCall was present AND already consumed return None
                // here and skip. Do NOT push inline at the arrival position —
                // that detaches the result from its call and lets a thought
                // interleave between them. Defer; the reposition pass attaches
                // each late result right after its call row.
                let call_id = &upd.tool_call_id.0;
                if let Some((_, Some(output))) = self.take_update(call_id) {
                    let (tool_name, _) = self.tools.get(call_id).cloned().unwrap_or_default();
                    // The tools map holds the window's own call frames, so an
                    // empty name means the call left the view: there is no call
                    // row here to attach the result to, and the state the
                    // earlier read handed on is the only place the call is
                    // still named. Reading that name in place of the map's is
                    // what lets one set of rules below cover both.
                    let out_of_view = tool_name.is_empty();
                    let tool = self.fold.pending_tool(call_id);
                    let title = match tool.as_deref() {
                        Some(tool) if out_of_view => tool,
                        _ => tool_name.as_str(),
                    };
                    // The result is in hand, so the call awaits nothing: no row
                    // that reads on has to name it.
                    self.fold.close_call(call_id);
                    // todo_write's result is boilerplate and draws no call row,
                    // so it has nothing to sit under here and would render as a
                    // raw row. The tool's name is what identifies it, and a call
                    // the state can no longer name leaves only the output itself,
                    // so the orphan is recognized by its distinctive old_todos
                    // field as well.
                    let orphan_todo = out_of_view && output.get("old_todos").is_some();
                    if title != "todo_write" && !orphan_todo {
                        if out_of_view {
                            // Render the call and its result where the result
                            // arrived: the call row before it stands in for the
                            // frames that left, so the result is never read as a
                            // line of its own. Only a tool that draws call rows
                            // gets one, so the same content reads the same
                            // whether or not the frames holding it are in view —
                            // a call the state cannot name gets one, because a
                            // result with nothing above it is the shape the
                            // reader cannot place.
                            if tool_renders_chip(title) {
                                self.out.push(call_out_of_view_row(
                                    call_id,
                                    tool.as_deref(),
                                    &output,
                                ));
                                self.ids.push(id.clone());
                            }
                            // A delegated call keeps the fold-group the
                            // in-view path draws: the group is what opens the
                            // child's own transcript, and the call's frames
                            // having left the view is no reason to lose that.
                            // The call's input went with those frames, so the
                            // group falls back to a generic type and an empty
                            // prompt, the shape the in-view path also takes for
                            // a call whose frame carries no input.
                            let row = subagent_line(&output, None).unwrap_or_else(|| {
                                tool_result_line(
                                    call_id,
                                    tool.as_deref().unwrap_or(""),
                                    &output,
                                    None,
                                )
                            });
                            self.out.push(row);
                            self.ids.push(id);
                        } else {
                            self.late_results.push(LateResult {
                                row: id,
                                call_id: call_id.clone(),
                                tool_name,
                                output,
                            });
                        }
                    }
                }
            }
            TranscriptFrame::Acpx(n) => match n.method {
                AcpxMethod::ContextCompactionBoundary => {
                    self.out
                        .push(TranscriptLine::System("compaction checkpoint".to_string()));
                    self.ids.push(id);
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
                    self.ids.push(id);
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
    fn emit_tool_call(&mut self, tc: &ToolCall, id: &RowId) {
        let call_id = &tc.tool_call_id.0;
        let upd = self.take_update(call_id);
        // The result is in hand, so the call awaits nothing: the update frame
        // that would read this name is consumed here, and a window that reads
        // on has no use for it. The rows below are skipped for a tool that
        // draws none, so the close runs ahead of them.
        if matches!(&upd, Some((_, Some(_)))) {
            self.fold.close_call(call_id);
        }
        if tc.title == "todo_write" {
            return;
        }
        if tool_renders_chip(&tc.title) {
            let outcome = upd
                .as_ref()
                .and_then(|(oc, _)| *oc)
                .unwrap_or(ToolOutcome::Running);
            let input = tc.raw_input.as_ref().cloned().unwrap_or(Value::Null);
            self.out.push(TranscriptLine::Tool {
                name: tool_user_facing_name(&tc.title, &input).to_string(),
                tool: tc.title.clone(),
                status: tool_call_brief(&tc.title, &input),
                invocation: tool_invocation(&tc.title, &input),
                outcome,
                call_id: call_id.clone(),
                body: String::new(),
                is_diff: false,
            });
            self.ids.push(id.clone());
        }
        if let Some((_, Some(output))) = upd {
            if let Some(sub) = subagent_line(&output, tc.raw_input.as_ref()) {
                self.out.push(sub);
                self.ids.push(id.clone());
            } else {
                self.out.push(tool_result_line(
                    call_id,
                    &tc.title,
                    &output,
                    tc.raw_input.as_ref(),
                ));
                self.ids.push(id.clone());
            }
        }
    }

    /// Reposition pass: attach each late result right after its matching call
    /// row so a result that arrived after a thought pulls back to its call
    /// (preserving call+result adjacency + input order). A result whose call
    /// draws no call row has nothing to attach to and renders on its own, the
    /// shape it also takes beside the call frame. Forward search for the first
    /// call row with the matching id; the harness ships one durable update per
    /// call, so at most one late result per id lands here, and the first match
    /// is the right one. Skip past any result rows already placed for THIS
    /// call_id so multiple late results for one call stack in arrival order
    /// without detaching an edit's diff from its call.
    fn finish(&mut self) {
        for late in take(&mut self.late_results) {
            let mut insert_at: Option<usize> = None;
            for (i, line) in self.out.iter().enumerate() {
                if let TranscriptLine::Tool { name, call_id, .. } = line
                    && name != "result"
                    && call_id == &late.call_id
                {
                    let mut j = i + 1;
                    while j < self.out.len()
                        && matches!(
                            &self.out[j],
                            TranscriptLine::Tool { name: nm, call_id: cid, .. }
                            if nm == "result" && cid == &late.call_id
                        )
                    {
                        j += 1;
                    }
                    insert_at = Some(j);
                    break;
                }
            }
            // No call row to sit under: a call whose frames left the view is
            // answered where the result arrives, so this is a call that draws
            // no row of its own.
            if let Some(pos) = insert_at {
                self.out.insert(
                    pos,
                    tool_result_line(&late.call_id, &late.tool_name, &late.output, None),
                );
                self.ids.insert(pos, late.row);
            } else {
                self.out.push(tool_result_line(
                    &late.call_id,
                    &late.tool_name,
                    &late.output,
                    None,
                ));
                self.ids.push(late.row);
            }
        }
    }
}

#[cfg(test)]
mod tests;
