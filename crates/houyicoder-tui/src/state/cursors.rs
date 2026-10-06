//! Typed cursor over the transcript frame log. A cursor names a position in
//! absolute frame space so front-of-window eviction cannot shift it: a Local
//! absolute index never moves when the resident vec drains from the front,
//! and a Server cursor holds the stream position its frame arrived on,
//! unique and monotonic while the issuing connection lives. Neither form is
//! a durable identity: stream numbering restarts with the service process,
//! and block anchors are Local-only.

use houyicoder_protocol::envelope::EventSeq;

/// A position in the frame log used by scroll and todo cursors. Local holds
/// an absolute frame index, monotonic across front drains; Server holds the
/// within-connection stream position a server frame carries. Resolution
/// returns the count of frames already consumed up to the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EventCursor {
    Server(EventSeq),
    Local(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two forms never collide: a stream position and a local index name
    /// different coordinate spaces, so the same number through each stays two
    /// distinct cursors.
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
