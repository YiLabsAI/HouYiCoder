//! Turn boundaries and the turn fold: where a turn opens and closes in an
//! ordered frame log, and the accumulation of one open turn's facts into the
//! summary row the transcript shows for it. Every predicate here reads frames
//! alone, with no run state: the fold backs readers that never watched the
//! turn live, so what opened a turn, what closed it, and whether a message
//! between the two was delivered into the running turn must all be facts of
//! the log. Row naming lives with the fold because the row is the fold's
//! output, and a name is only useful while it identifies the same turn in
//! every read that carries it.

use std::ops::Range;

use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::frontend::session_update::SessionUpdate;

use super::TranscriptFrame;
use super::frame_payload::chunk_text;
use super::tool_renders_chip;
use crate::records::TranscriptLine;

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
fn is_user_frame(frame: &TranscriptFrame) -> bool {
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
fn open_turn_before<F: AsRef<TranscriptFrame>>(log: &[F], start: usize) -> Option<usize> {
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

/// Where folded summary rows take their durable name. The name is only
/// useful while it identifies the same turn in every read that carries it,
/// and what can promise that differs by the log being read.
pub(super) enum RowNames<'a> {
    /// The live frame log, whose rows are named by absolute frame position:
    /// the caller tracks the absolute index of log[0] and adds the fold's
    /// end position to it, so a row keeps its name while the log's front
    /// drains.
    FrameIndex { abs_base: usize },
    /// A durable log read, one formatted name per frame in log order: a row
    /// takes the name at the frame that closed its turn, which no read can
    /// shift because it belongs to the event, not to the window. The reader
    /// formats the names, since the log's identity type is the reader's; the
    /// fold only requires that names run parallel to the frames.
    EventName(&'a [String]),
}

/// The facts one turn accumulates for its summary row, gathered as the
/// projection walks that turn's frames. The row is emitted where the turn
/// ends, so the summary lands under the answer it describes.
pub(super) struct TurnFold<'a, F: AsRef<TranscriptFrame>> {
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
    /// Where the row of a closed turn takes its durable name.
    names: RowNames<'a>,
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
    pub(super) fn new(log: &'a [F], window: Range<usize>, names: RowNames<'a>) -> Self {
        let opened_at = open_turn_before(log, window.start);
        let mut fold = Self {
            log,
            // The turn the window opens inside has ended, so far, at the frame
            // that opened it; the read-back pass advances this over the facts
            // it absorbs ahead of the window, and the walk advances it over
            // the facts the window holds.
            ended_at: opened_at.unwrap_or(window.start),
            opened_at,
            names,
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
    /// These frames render no rows of their own here. The pass also advances
    /// where the turn currently ends, by the same rule the walk over the
    /// window's frames follows, so a turn that closes at the window's first
    /// frame is named by its own newest fact rather than by that frame.
    fn absorb_ahead(&mut self, start: usize) {
        let Some(opened_at) = self.opened_at else {
            return;
        };
        let log = self.log;
        for (offset, frame) in log[opened_at + 1..start].iter().enumerate() {
            let frame = frame.as_ref();
            if moves_turn_end(frame) {
                self.ended_at = opened_at + 1 + offset;
            }
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
    pub(super) fn note(&mut self, abs: usize, frame: &TranscriptFrame) -> Option<TranscriptLine> {
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
        if moves_turn_end(frame) {
            self.ended_at = abs;
        }
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
    pub(super) fn close(&mut self, ms: Option<u64>) -> Option<TranscriptLine> {
        self.opened_at.take()?;
        if self.reasoning.is_empty() && self.calls == 0 {
            return None;
        }
        let reasoning = (!self.reasoning.is_empty()).then(|| std::mem::take(&mut self.reasoning));
        Some(TranscriptLine::ThoughtFor {
            ms,
            reasoning,
            tool_summary: self.tool_summary(),
            turn_id: self.row_name(),
        })
    }

    /// The durable name of the closing turn's row: an absolute frame position
    /// in a live log, the reader's own event name in a durable read.
    fn row_name(&self) -> String {
        match &self.names {
            RowNames::FrameIndex { abs_base } => format!("f{}", abs_base + self.ended_at),
            RowNames::EventName(names) => {
                // ended_at is a log position and the names run parallel to
                // the log. A degenerate empty window at the log's end would
                // hold a position one past the last frame; the newest name is
                // the row's nearest durable anchor there.
                let at = self.ended_at.min(names.len().saturating_sub(1));
                names
                    .get(at)
                    .expect("row names run parallel to the frame log")
                    .clone()
            }
        }
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
