//! Typed cursor over the transcript frame log. A cursor names a position in
//! absolute frame space so front-of-window eviction cannot shift it: a Local
//! absolute index never moves when the resident vec drains from the front,
//! and a Server event seq is durable across resume. The dual-anchor mirrors
//! the block identity anchor so a cursor and the block it lands on share one
//! notion of a stable frame position.

use houyicoder_protocol::envelope::EventSeq;

/// A position in the frame log used by scroll and todo cursors. Local holds
/// an absolute frame index, monotonic across front drains; Server holds the
/// durable event seq a server frame carries. Resolution returns the count of
/// frames already consumed up to the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EventCursor {
    Server(EventSeq),
    Local(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two anchors never collide: a server seq and a local index name
    /// different identity spaces, so a cursor resolved by one cannot match a
    /// block anchored on the other.
    #[test]
    fn test_cursor_anchor_distinct() {
        let server = EventCursor::Server(EventSeq(7));
        let local = EventCursor::Local(7);
        assert_ne!(server, local);
    }

    /// A cursor is Copy, so call sites read it without borrowing the owner.
    #[test]
    fn test_cursor_copy() {
        let a = EventCursor::Local(3);
        let b = a;
        assert_eq!(a, b);
    }
}
