use std::sync::mpsc;

use super::*;
use crate::composition;

/// Both callers guard before dispatching, and two tests catch a raised
/// budget through those guards, so this assert is a backstop for a caller
/// that forgets one: told in debug rather than left to orphan the chunk
/// already running. No worker is needed to trip it, since the record on
/// file is what the count reads.
#[test]
#[should_panic(expected = "past the budget")]
fn test_dispatch_enforces_budget() {
    let mut app = composition::app();
    let (_tx, rx) = mpsc::channel();
    *app.sessions
        .current
        .index_build
        .pending_index_chunk
        .borrow_mut() = Some(PendingIndexChunk::new(rx));

    app.dispatch_index_chunk();
}
