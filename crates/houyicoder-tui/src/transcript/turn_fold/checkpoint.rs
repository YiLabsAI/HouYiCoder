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

/// What one window read hands the next window about the turn it opened into.
///
/// The state carries no frame content: reasoning text stays in the frames
/// that hold it, and a window that has none left reports the text it lost by
/// its size. That is what keeps the state bounded however long the turn ran,
/// and it is why a summary row can say a turn reasoned without repeating
/// what it reasoned.
///
/// A fact belongs here only when the window that closes the turn cannot read
/// it from any frame the window holds: each field restates what the turn's
/// frames carry, kept for the windows that no longer hold them.
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
}

impl TurnCheckpoint {
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
    }

    /// Close the turn: the next frame that opens one opens a new turn.
    pub(super) fn close(&mut self) {
        self.opened_at = None;
        self.newest_fact = None;
        self.reasoning_chars = 0;
        self.tool_counts.clear();
        self.calls = 0;
    }

    /// Move where the turn's newest fact sits, as a frame inside the window
    /// being walked supplies it.
    pub(super) fn moved_to(&mut self, at: RowId) {
        self.newest_fact = Some(at);
    }
}
