//! Bounded paging over a session's durable history: the position a read
//! resumes at, one page of records, and the walk that reads every page from a
//! position to the end of what the store can serve.

use houyicoder_context::{ContextError, SessionId, SessionLogEntry};

use super::SessionLog;

/// Where a paged trajectory read resumes.
///
/// The index counts durable records on the session's current epoch: 0 is the
/// record the newest clear boundary begins at. It is the record's ordinal in
/// the durable log, which is what makes it a position a reader can name once
/// and return to after a restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrajectoryPosition {
    /// Absolute index of the next record to read.
    pub index: usize,
    /// The byte position the index begins at in the durable log, when this
    /// position came from a log read. None while the resident window answers.
    pub byte_offset: Option<u64>,
}

impl TrajectoryPosition {
    /// The position a read of the whole history starts at.
    pub fn start() -> Self {
        Self::default()
    }
}

/// The records one page read asks for. Each page is dropped once the reader
/// has seen it, so this is the memory a walk of the whole history costs.
const PAGE_EVENTS: usize = 512;

/// One bounded page of a session's durable history.
#[derive(Debug, Default, Clone)]
pub struct TrajectoryPage {
    /// Durable records in append order, at most the limit the reader asked
    /// for. Streamed text deltas are absent: the log carries none, so a read
    /// that serves part of its answer from the log cannot reproduce them.
    pub events: Vec<SessionLogEntry>,
    /// Where the next page continues.
    pub next: TrajectoryPosition,
    /// Bytes this page passed over because no complete record fit the log's
    /// read window. A record wider than that window cannot be parsed, so the
    /// reader reports the span instead of presenting a gap as a neighbour.
    /// Zero when the page read every record it covered.
    pub skipped_bytes: u64,
}

/// A session's durable history as successive bounded pages, read from an
/// absolute position to the end of what the store can serve.
///
/// A reader that must see every record reads these pages rather than the
/// resident tail: the tail holds only the newest rows, so a reader that took
/// it for the history would answer with the part memory happens to hold and
/// would report a session whose front was dropped as a whole one. Each page is
/// released as the next is read, so reading a history this way costs one page
/// of memory however long the history is.
///
/// The read ends at the first page without records, which is how a store
/// reports a history with nothing left to serve.
pub struct TrajectoryPages<'a> {
    log: &'a dyn SessionLog,
    session: SessionId,
    position: TrajectoryPosition,
    done: bool,
}

impl<'a> TrajectoryPages<'a> {
    /// The pages of a session's durable history, from a position to its end.
    pub fn new(log: &'a dyn SessionLog, session: SessionId, from: TrajectoryPosition) -> Self {
        Self {
            log,
            session,
            position: from,
            done: false,
        }
    }
}

impl Iterator for TrajectoryPages<'_> {
    type Item = Result<TrajectoryPage, ContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let page = match self
            .log
            .trajectory_page(self.session, self.position, PAGE_EVENTS)
        {
            Ok(page) => page,
            Err(e) => {
                // A failed read ends the walk: retrying the same position
                // would report the same failure until something else changes.
                self.done = true;
                return Some(Err(e));
            }
        };
        if page.events.is_empty() {
            self.done = true;
            return None;
        }
        // A page that did not move the position describes records the store
        // cannot serve past, so the read stops after reporting it once.
        let moved = page.next != self.position;
        self.position = page.next;
        if !moved {
            self.done = true;
        }
        Some(Ok(page))
    }
}
