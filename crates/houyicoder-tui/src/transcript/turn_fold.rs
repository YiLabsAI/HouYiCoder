//! Turn boundaries and the turn fold: where a turn opens and closes in an
//! ordered frame log, and the accumulation of one open turn's facts into the
//! summary row the transcript shows for it. Every predicate here reads frames
//! alone, with no run state: the fold backs readers that never watched the
//! turn live, so what opened a turn, what closed it, and whether a message
//! between the two was delivered into the running turn must all be facts of
//! the log. Row naming lives with the fold because the row is the fold's
//! output, and a name is only useful while it identifies the same turn in
//! every read that carries it.
//!
//! A turn outlives the window that holds its frames: the window a read is
//! handed may open past the frame that opened the turn, and the frames
//! between may already have been drained. The fold therefore takes the state
//! a previous window produced for the turn it opened into, and hands on the
//! state at its own end, so one turn is folded once however many windows it
//! spans.

mod checkpoint;

use std::cmp::Reverse;
use std::mem::take;
use std::ops::Range;

use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::frontend::session_update::SessionUpdate;

use super::SequencedFrame;
use super::TranscriptFrame;
use super::call_rows::tool_renders_chip;
use super::frame_payload::chunk_text;
use crate::records::TranscriptLine;
use crate::row_id::RowId;

pub use checkpoint::TurnCheckpoint;

/// The duration a run-completion record carries. A record without a readable
/// one closes the turn with no duration rather than a claimed zero.
fn recorded_ms(params: &serde_json::Value) -> Option<u64> {
    params.get("ms").and_then(|v| v.as_u64())
}

/// Whether a frame moves where the open turn currently ends — the position
/// that names the turn's row once it closes. The turn's own facts move it:
/// its reasoning, its answer, its tool calls, and the record that closes it.
/// A message delivered into the turn and a row the frontend raised are not
/// facts of the turn and do not move it. Both the walk over the window's
/// frames and the read-back over the frames ahead of the window follow this
/// one rule, which is what makes a row's name independent of where a read
/// started.
fn moves_turn_end(frame: &TranscriptFrame) -> bool {
    !matches!(
        frame,
        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_)) | TranscriptFrame::Frontend(_)
    )
}

/// Whether the frame is a run-completion record: the marker that names where a
/// turn ended.
fn is_run_completed(frame: &TranscriptFrame) -> bool {
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
fn opens_turn<F: AsRef<TranscriptFrame>>(frames: &[F], at: usize) -> bool {
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
/// host sends the mark right after the message it belongs to; the fold reads
/// it to place that message inside the turn that was already running.
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

/// Where the frame that decides how a message reads sits, for the message at
/// the given position: the first frame after it that is not a row the frontend
/// raised. A mark for the message sits there, and any other frame shows the
/// message opened a turn of its own. A read derives a message's rows from that
/// frame, so those rows hold only while the same frame still decides. None when
/// no frame follows, or when no message sits there.
pub(crate) fn delivered_marker_at<F: AsRef<TranscriptFrame>>(
    frames: &[F],
    at: usize,
) -> Option<usize> {
    if !frames.get(at).is_some_and(|sf| is_user_frame(sf.as_ref())) {
        return None;
    }
    frames
        .get(at + 1..)
        .unwrap_or_default()
        .iter()
        .position(|sf| !matches!(sf.as_ref(), TranscriptFrame::Frontend(_)))
        .map(|i| at + 1 + i)
}

/// Whether the log marks the user message at at as delivered into the turn
/// that was already running. The mark sits beside the message, but a row the
/// frontend raises while the message is in hand can land between the two:
/// the mark is read past those rows, so one raised there cannot leave the
/// message reading as a turn of its own for the rest of the session.
fn message_marked_delivered<F: AsRef<TranscriptFrame>>(frames: &[F], at: usize) -> bool {
    delivered_marker_at(frames, at).is_some_and(|at| marks_delivery(frames[at].as_ref()))
}

/// Where the turn a window starts inside begins: the message that opened it.
/// A user frame is not always an opening — a message the log marks as
/// delivered into the running turn belongs to the turn already running, so
/// the walk continues past it. The search stops at the first undelivered
/// message; a record before it ends the turn instead, and there the window
/// opens between turns, with no turn to fold. A window cut inside a turn
/// still folds that turn, so its row keeps its place rather than vanishing
/// whenever the oldest frames of the view fall inside a turn.
fn open_turn_before<F: AsRef<TranscriptFrame>>(log: &[F], start: usize) -> Option<usize> {
    // One pass back over the frames, so the work grows with how many frames
    // the window leaves behind rather than restarting the search at each
    // message the running turn absorbed: a turn carrying many queued messages
    // would otherwise walk its own prefix once per message.
    for at in (0..start).rev() {
        let frame = log[at].as_ref();
        // A record here closed the turn before this position, so the window
        // opens between turns: no turn to fold.
        if is_run_completed(frame) {
            return None;
        }
        if !is_user_frame(frame) {
            continue;
        }
        // A message the log marks as delivered into the running turn did not
        // open it, so the turn begins further back. Reading it as the opening
        // would cut the turn's earlier facts out of the row.
        if opens_turn(log, at) {
            return Some(at);
        }
    }
    None
}

/// Where folded summary rows take their identity. The identity is only
/// useful while it names the same turn in every read that carries it, and
/// what can promise that differs by the log being read.
pub(super) enum RowNames<'a> {
    /// The live frame log. A row whose closing frame came from a durable
    /// entry takes that entry's identity off the frame itself; a row no entry
    /// named takes the absolute position where its turn ended, so the name
    /// holds still while the log's front drains.
    Live {
        log: &'a [SequencedFrame],
        abs_base: usize,
    },
    /// A log of bare frames, where no frame carries an entry identity: every
    /// row takes the absolute position where its turn ended, which holds still
    /// while the log's front drains.
    Position { abs_base: usize },
    /// A durable log read, one row identity per frame in log order: a row
    /// takes the identity at the frame that closed its turn, which no read
    /// can shift because it belongs to the entry, not to the window. The
    /// reader derives the identities through the same function the live path
    /// uses; the fold only requires that they run parallel to the frames.
    EventName(&'a [RowId]),
}

impl RowNames<'_> {
    /// The identity of the rows the frame at log position at produces: the
    /// entry that produced the frame when the read carries one, the absolute
    /// position otherwise. A degenerate window at the log's end can name a
    /// position one past the last frame; the nearest identity is the row's
    /// anchor there, and a position-named row keeps the render going rather
    /// than failing on a frame that is not there.
    pub(super) fn name_at(&self, at: usize) -> RowId {
        match self {
            RowNames::Live { log, abs_base } => match log.get(at) {
                Some(sf) => RowId::of(sf.as_ref(), sf.event_id.as_ref(), (*abs_base + at) as u64),
                None => RowId::local((*abs_base + at) as u64),
            },
            RowNames::Position { abs_base } => RowId::local((*abs_base + at) as u64),
            RowNames::EventName(names) => {
                let at = at.min(names.len().saturating_sub(1));
                names
                    .get(at)
                    .expect("row names run parallel to the frame log")
                    .clone()
            }
        }
    }
}

/// The facts one turn accumulates for its summary row, gathered as the
/// fold walks that turn's frames. The row is emitted where the turn
/// ends, so the summary lands under the answer it describes.
pub(super) struct TurnFold<'a, F: AsRef<TranscriptFrame>> {
    /// The whole log, for what a window cannot carry on its own: which turn the
    /// window opens inside, what that turn did before the window began, and
    /// whether the message beside it was delivered into a running turn.
    log: &'a [F],
    /// Where the row of a closed turn takes its durable name.
    names: RowNames<'a>,
    /// Where the window ends, in log positions. The frame after it decides how
    /// the window's last message reads, so the state handed on records that
    /// frame for the reader that may reuse these rows.
    ends_at: usize,
    /// Reasoning text the open turn's frames carried inside this window. Text
    /// the state passed on is counted there rather than repeated here, so a
    /// window holds only the text its own frames brought.
    reasoning: String,
    /// The open turn's facts, including those the window that opened it
    /// handed on.
    state: TurnCheckpoint,
}

impl<'a, F: AsRef<TranscriptFrame>> TurnFold<'a, F> {
    /// Fold a window of the log, continuing the turn a previous window ended
    /// inside when state carries one. Frames of earlier turns are not folded;
    /// the caller pushes the rows of the turns they closed. Without carried
    /// state the fold itself reads back what the open turn did before the
    /// window, so a caller that hands a window of the whole log needs to pass
    /// nothing.
    pub(super) fn new(
        log: &'a [F],
        window: Range<usize>,
        names: RowNames<'a>,
        carried: Option<TurnCheckpoint>,
    ) -> Self {
        let mut fold = Self {
            log,
            names,
            ends_at: window.end,
            reasoning: String::new(),
            state: TurnCheckpoint::default(),
        };
        match carried {
            Some(state) => fold.state = state,
            None => fold.absorb_ahead(window.start),
        }
        fold
    }

    /// Fold what the open turn did before the window, so the row summarizes the
    /// turn rather than the part of it this window happens to hold. Without it
    /// a window whose oldest frame is the turn's own record finds no reasoning
    /// and no calls to summarize, and drops a row the whole-log read writes.
    /// These frames render no rows of their own here. The pass also moves where
    /// the turn currently ends, by the same rule the walk over the window's
    /// frames follows, so a turn that closes at the window's first frame is
    /// named by its own newest fact rather than by that frame.
    fn absorb_ahead(&mut self, start: usize) {
        let Some(opened_at) = open_turn_before(self.log, start) else {
            return;
        };
        self.state.open(self.names.name_at(opened_at));
        let log = self.log;
        for (offset, frame) in log[opened_at + 1..start].iter().enumerate() {
            let frame = frame.as_ref();
            if moves_turn_end(frame) {
                self.state
                    .moved_to(self.names.name_at(opened_at + 1 + offset));
            }
            if let TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) = frame
                && upd.fields.raw_output.is_some()
            {
                // A call this read-back sees answered leaves the state, as it
                // does in the walk: the names are kept for a window that
                // renders a result with the call's own frames out of view, and
                // the bound on them is spent by the calls still awaiting one.
                // The close runs where the result frame sits, so the calls
                // this read-back walks afterwards find the room it freed.
                self.state.pending_calls.close(&upd.tool_call_id.0);
            }
            self.gather(frame);
        }
    }

    /// The state at this window's end, for the read of the next window. The
    /// frame that decides how the window's last message reads travels with it:
    /// a reader that reuses these rows has to know whether the decision was
    /// taken under the same frame.
    pub(super) fn checkpoint(&self) -> TurnCheckpoint {
        let mut state = self.state.clone();
        state.delivered_marker = self
            .ends_at
            .checked_sub(1)
            .and_then(|at| delivered_marker_at(self.log, at))
            .map(|at| self.names.name_at(at));
        state
    }

    /// The tool a call the open turn made was for, while its result has not
    /// arrived: what a window that no longer holds the call's frames renders
    /// that result under.
    pub(super) fn pending_tool(&self, call_id: &str) -> Option<String> {
        self.state.pending_calls.tool(call_id).map(str::to_string)
    }

    /// Forget a call once its result is on screen: the call no longer awaits
    /// one, and no window that reads on has to name it.
    pub(super) fn close_call(&mut self, call_id: &str) {
        self.state.pending_calls.close(call_id);
    }

    /// Whether the message at abs was delivered into the running turn.
    fn delivered_into_turn(&self, abs: usize) -> bool {
        message_marked_delivered(self.log, abs)
    }

    /// Fold one frame into the open turn. A user message opens a turn and ends
    /// the one before it, unless the log marks it as delivered into that turn;
    /// a completion record ends the turn it followed and carries that run's
    /// duration. Returns the row for a turn this frame ended, with its
    /// identity, for the caller to push where the turn ended.
    pub(super) fn note(
        &mut self,
        abs: usize,
        frame: &TranscriptFrame,
    ) -> Option<(TranscriptLine, RowId)> {
        match frame {
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_)) => {
                let closed = if self.delivered_into_turn(abs) {
                    None
                } else {
                    self.close(None)
                };
                if !self.state.is_open() {
                    self.state.open(self.names.name_at(abs));
                    self.reasoning.clear();
                }
                return closed;
            }
            TranscriptFrame::Acpx(n) if n.method == AcpxMethod::ContextRunCompleted => {
                self.state.moved_to(self.names.name_at(abs));
                return self.close(recorded_ms(&n.params));
            }
            // A row the frontend raises is not a fact of the turn it sits in:
            // it does not move where the turn ended, which names the row.
            TranscriptFrame::Frontend(_) => return None,
            _ => {}
        }
        self.gather(frame);
        if moves_turn_end(frame) {
            self.state.moved_to(self.names.name_at(abs));
        }
        None
    }

    /// Collect what one frame of the open turn contributes to its summary: its
    /// reasoning text and the tool calls it made. Read for the frames the
    /// window holds and for those it reads back to reach the turn's opening.
    fn gather(&mut self, frame: &TranscriptFrame) {
        if !self.state.is_open() {
            return;
        }
        match frame {
            TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(chunk)) => {
                let text = chunk_text(chunk);
                self.state.reasoning_chars += text.chars().count() as u64;
                self.reasoning.push_str(text);
            }
            TranscriptFrame::Session(SessionUpdate::ToolCall(call)) => {
                // Every call's name is kept for the window that renders its
                // result; only the calls that draw a row of their own count
                // toward the turn's summary.
                self.state
                    .pending_calls
                    .add(&call.tool_call_id.0, &call.title);
                if tool_renders_chip(&call.title) {
                    self.state.tool_counts.add(&call.title);
                    self.state.calls += 1;
                }
            }
            _ => {}
        }
    }

    /// Close the open turn and return its row with its identity: None when no
    /// turn is open, or when the turn has nothing to summarize. A plain reply
    /// renders no row whose expand affordance would lead to nothing.
    pub(super) fn close(&mut self, ms: Option<u64>) -> Option<(TranscriptLine, RowId)> {
        if !self.state.is_open() {
            return None;
        }
        // An open turn always names a newest fact: opening one names the frame
        // that opened it, and every fact since moves the name on.
        let id = self.state.newest_fact.clone()?;
        let text = take(&mut self.reasoning);
        let earlier = self
            .state
            .reasoning_chars
            .saturating_sub(text.chars().count() as u64);
        let tool_summary = self.tool_summary();
        self.state.close();
        if text.is_empty() && earlier == 0 && tool_summary.is_none() {
            return None;
        }
        Some((
            TranscriptLine::ThoughtFor {
                ms,
                reasoning: reasoning_body(earlier, text),
                tool_summary,
                turn_id: id.clone(),
            },
            id,
        ))
    }

    /// The identity of the rows the frame at log position at produces, for
    /// the walk that emits them beside the fold that names the turns.
    pub(super) fn name_at(&self, at: usize) -> RowId {
        self.names.name_at(at)
    }

    /// The one-line tool summary, grouped by tool with the most-used first.
    /// A turn that called one tool reads as its own count; several keep the
    /// breakdown.
    fn tool_summary(&self) -> Option<String> {
        if self.state.calls == 0 {
            return None;
        }
        let mut named = self.state.tool_counts.named().to_vec();
        named.sort_by_key(|(_, count)| Reverse(*count));
        let other = self.state.tool_counts.other();
        if other == 0
            && let [(tool, count)] = named.as_slice()
        {
            return Some(format!("ran {count} {tool}"));
        }
        let mut parts: Vec<String> = named
            .iter()
            .map(|(tool, count)| format!("{count} {tool}"))
            .collect();
        if other > 0 {
            parts.push(format!("{other} other"));
        }
        Some(format!(
            "ran {} tools ({})",
            self.state.calls,
            parts.join(", ")
        ))
    }
}

/// The text a summary row expands into: what this window's frames carried,
/// led by a note counting the reasoning that left the window.
fn reasoning_body(earlier: u64, text: String) -> Option<String> {
    if earlier == 0 {
        return (!text.is_empty()).then_some(text);
    }
    let note =
        format!("({earlier} characters of this turn's reasoning are earlier in the transcript)");
    if text.is_empty() {
        return Some(note);
    }
    Some(format!("{note}\n{text}"))
}
