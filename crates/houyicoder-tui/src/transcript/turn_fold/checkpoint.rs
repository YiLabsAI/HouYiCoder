//! The facts of an open turn that a window read carries past its own frames.
//!
//! A turn's summary row is written where the turn ends, which can lie in a
//! window other than the one holding the turn's earlier facts: the log a read
//! is handed may open past the frame that opened the turn, and the frames
//! between may have been drained. The state here is what survives that
//! boundary — identities, counts, and totals, never frame content — so a
//! window can finish a turn it did not open.

use crate::row_id::RowId;

/// How many tools a turn's summary lists individually. Past the bound the rest
/// fold into one bucket, because this state has to stay the same size however
/// many distinct tools a turn called: a turn that outruns the bound reports
/// its remainder as a single count while the total stays exact.
const NAMED_TOOLS: usize = 8;

/// Tool calls counted by tool: the named tools with their counts, and one
/// bucket for every tool past the bound. The bucket keeps the total honest
/// when the names no longer fit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ToolCounts {
    named: Vec<(String, u32)>,
    other: u32,
}

impl ToolCounts {
    /// Count one call of this tool.
    pub(super) fn add(&mut self, tool: &str) {
        if let Some(slot) = self.named.iter_mut().find(|(name, _)| name == tool) {
            slot.1 += 1;
            return;
        }
        if self.named.len() < NAMED_TOOLS {
            self.named.push((tool.to_string(), 1));
        } else {
            self.other += 1;
        }
    }

    /// The named tools, each with its count.
    pub(super) fn named(&self) -> &[(String, u32)] {
        &self.named
    }

    /// Calls of tools past the named bound.
    pub(super) fn other(&self) -> u32 {
        self.other
    }

    /// Drop every count, for a new turn.
    pub(super) fn clear(&mut self) {
        self.named.clear();
        self.other = 0;
    }
}

/// How many calls a turn may hold unanswered at once. The bound caps the state,
/// not the turn: a turn that leaves more calls unanswered than this reports the
/// remainder without a name rather than growing with the batch.
const PENDING_CALLS: usize = 32;

/// The calls the open turn made whose result has not arrived, each with the
/// tool it called. A result can reach a window whose frames no longer hold its
/// call, and the name here is what lets that window show the call the result
/// answers: without it the result arrives with nothing above it. Past the bound
/// the call is not kept, and a result for it still renders — the row says the
/// call is out of view instead of naming it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PendingCalls {
    calls: Vec<(String, String)>,
}

impl PendingCalls {
    /// Record a call the turn made, before its result arrives.
    pub(super) fn add(&mut self, call_id: &str, tool: &str) {
        if self.calls.iter().any(|(id, _)| id == call_id) || self.calls.len() >= PENDING_CALLS {
            return;
        }
        self.calls.push((call_id.to_string(), tool.to_string()));
    }

    /// Drop the call whose result arrived.
    pub(super) fn close(&mut self, call_id: &str) {
        self.calls.retain(|(id, _)| id != call_id);
    }

    /// The tool of a call still awaiting its result.
    pub(super) fn tool(&self, call_id: &str) -> Option<&str> {
        self.calls
            .iter()
            .find(|(id, _)| id == call_id)
            .map(|(_, tool)| tool.as_str())
    }

    /// Drop every call, for a new turn: a turn's calls are the turn's, and the
    /// next turn's results belong to the next turn's calls.
    pub(super) fn clear(&mut self) {
        self.calls.clear();
    }
}

/// What one window read hands the next window about the turn it opened into.
///
/// The state carries no frame content: reasoning text stays in the frames
/// that hold it, and a window that has none left reports the text it lost by
/// its size. That is what keeps the state bounded however long the turn ran,
/// and it is why a summary row can say a turn reasoned without repeating
/// what it reasoned.
///
/// A fact belongs here only when a read cannot take it from the frames the
/// window covers: the turn's identities, counts, and totals restate what its
/// earlier frames carried, for the windows that no longer hold them, and the
/// delivery decision restates the frame that follows the window's last one,
/// which decides how that message reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnCheckpoint {
    /// Identity of the frame that opened the turn the window ends inside.
    /// None means the window ends between turns: the next window opens
    /// whatever turn its own frames open.
    pub(super) opened_at: Option<RowId>,
    /// Identity of the open turn's newest fact, which names the turn's summary
    /// row once the turn closes. Opening a turn names the frame that opened
    /// it, and every fact since moves the name on, so an open turn always has
    /// one.
    pub(super) newest_fact: Option<RowId>,
    /// Characters of reasoning text the turn's frames carried, counted rather
    /// than kept, so a window holding none of that text can still report how
    /// much of it left.
    pub(super) reasoning_chars: u64,
    pub(super) tool_counts: ToolCounts,
    /// Tool calls the turn made, the total the named counts add up to.
    pub(super) calls: u32,
    /// Calls the turn made whose result has not arrived.
    pub(super) pending_calls: PendingCalls,
    /// Identity of the frame that decides how the message this window ends on
    /// reads: the mark that says the message joined the turn already running,
    /// or the frame that shows there is none. A reader reuses these rows only
    /// while the same frame still decides, so a message that first read as a
    /// turn of its own stops reading that way once its mark arrives. None when
    /// the window does not end on a message.
    pub(super) delivered_marker: Option<RowId>,
}

impl TurnCheckpoint {
    /// The frame that decided how the message this state's read ended on reads,
    /// for a reader that reuses the rows derived under that decision.
    pub(crate) fn delivered_marker(&self) -> Option<&RowId> {
        self.delivered_marker.as_ref()
    }
    /// Whether the window ends inside a turn: its frames opened one that no
    /// frame closed.
    pub(super) fn is_open(&self) -> bool {
        self.opened_at.is_some()
    }

    /// Open a turn at the frame with this identity, with no facts yet.
    pub(super) fn open(&mut self, at: RowId) {
        self.opened_at = Some(at.clone());
        self.newest_fact = Some(at);
        self.reasoning_chars = 0;
        self.tool_counts.clear();
        self.calls = 0;
        self.pending_calls.clear();
    }

    /// Close the turn: the next frame that opens one opens a new turn.
    pub(super) fn close(&mut self) {
        self.opened_at = None;
        self.newest_fact = None;
        self.reasoning_chars = 0;
        self.tool_counts.clear();
        self.calls = 0;
        self.pending_calls.clear();
    }

    /// Move where the turn's newest fact sits, as a frame inside the window
    /// being walked supplies it.
    pub(super) fn moved_to(&mut self, at: RowId) {
        self.newest_fact = Some(at);
    }
}
