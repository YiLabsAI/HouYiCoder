//! Typed cursor over the transcript frame log. A cursor names a position in
//! absolute frame space so front-of-window eviction cannot shift it: a Local
//! absolute index never moves when the resident vec drains from the front,
//! and a Durable cursor holds the row identity of the frame it anchored on,
//! which names a log entry and stays resolvable across re-reads.

use crate::row_id::RowId;

/// A position in the frame log used by scroll and todo cursors. Local holds
/// an absolute frame index, monotonic across front drains; Durable holds the
/// row identity its frame carried. Resolution returns the count of frames
/// already consumed up to the anchor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum EventCursor {
    Durable(RowId),
    Local(u64),
}

#[cfg(test)]
mod tests;
